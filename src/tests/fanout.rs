pub mod support;

use relay::fanout::{self, FanoutMode, FanoutState, FanoutStore};
use relay::lifecycle::{
    ClaimManagedAttach, ClaimOutcome, ExecutionBackend, LifecycleStore, ManagedState,
    PendingAttachSpec, ProcessObservation, RequiredScope, StartGeneration, TerminalAction,
};
use relay::protocol::{
    ClaimOrigin, ClaimState, ClaimStatusV1, DeliveryState, MessageKind, ObjectFormat,
    ProtocolStore, TerminalStatus, WorkerResultV1,
};
use relay::store;
use relay::workspace::authority::{
    AuthorityRootProvider, SystemAuthorityRootProvider, WorkspaceAuthority,
};
use relay::workspace::git::OpenedRepository;
use relay::workspace::repository_gate::RepositoryGate;
use rustix::fs::{FlockOperation, flock};
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Barrier};
use std::thread;
use support::fanout::{count_reservation_leaves, mutate_fanout_record};
use support::fresh_home;
use tinyjson::JsonValue;

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn init_repo(home: &Path) -> PathBuf {
    let repo = home.join("repo");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "relay@example.test"]);
    git(&repo, &["config", "user.name", "Relay Test"]);
    fs::write(repo.join("shared.txt"), "base\n").unwrap();
    git(&repo, &["add", "shared.txt"]);
    git(&repo, &["commit", "-qm", "base"]);
    repo
}

fn seed_entry(home: &Path, id: &str, cwd: &Path) {
    for dir in ["mailbox", "markers", "watchers", "locks"] {
        fs::create_dir_all(home.join(dir)).unwrap();
    }
    let registry_path = home.join("registry.json");
    let mut root = fs::read_to_string(&registry_path)
        .ok()
        .and_then(|raw| raw.parse::<JsonValue>().ok())
        .and_then(|value| value.get::<HashMap<String, JsonValue>>().cloned())
        .unwrap_or_default();
    let mut agents = root
        .remove("agents")
        .and_then(|value| value.get::<HashMap<String, JsonValue>>().cloned())
        .unwrap_or_default();
    let mut entry = HashMap::new();
    entry.insert("id".into(), JsonValue::from(id.to_string()));
    entry.insert(
        "dir".into(),
        JsonValue::from(cwd.to_string_lossy().into_owned()),
    );
    entry.insert("name".into(), JsonValue::from(()));
    entry.insert("tool".into(), JsonValue::from("claude".to_string()));
    entry.insert("lastSeen".into(), JsonValue::from(store::iso_now()));
    entry.insert("server".into(), JsonValue::from(()));
    entry.insert("spawned_via".into(), JsonValue::from(()));
    agents.insert(id.to_string(), JsonValue::from(entry));
    root.insert("agents".into(), JsonValue::from(agents));
    root.entry("names".into())
        .or_insert_with(|| JsonValue::from(HashMap::new()));
    fs::write(registry_path, JsonValue::from(root).format().unwrap()).unwrap();
}

fn bind_pending_managed_fanout_worker(
    fanout: &FanoutStore,
    record_id: &str,
    runtime_session_id: &str,
    cwd: &Path,
) -> (String, String, String) {
    let worker = store::uuid_v4();
    let generation = store::uuid_v4();
    let token = format!("{}{}", store::uuid_v4(), store::uuid_v4());
    let lifecycle = LifecycleStore::new(fanout.root().to_path_buf());
    lifecycle
        .create_pending(
            PendingAttachSpec {
                worker_id: worker.clone(),
                generation: generation.clone(),
                expected_runtime_session_id: Some(runtime_session_id.to_string()),
                expected_tool: "claude".to_string(),
                expected_cwd: cwd.to_string_lossy().into_owned(),
                expires_at_ms: store::now_ms() + 30_000,
                required_scope: RequiredScope::ProcessOnly,
                execution: ExecutionBackend::SupervisorOwnedProcess,
            },
            &token,
        )
        .unwrap();
    fanout
        .bind_managed(record_id, &worker, &generation)
        .unwrap();
    (worker, generation, token)
}

fn activate_managed_fanout_worker(
    fanout: &FanoutStore,
    record_id: &str,
    runtime_session_id: &str,
    cwd: &Path,
) -> (String, String) {
    let (worker, generation, token) =
        bind_pending_managed_fanout_worker(fanout, record_id, runtime_session_id, cwd);
    let lifecycle = LifecycleStore::new(fanout.root().to_path_buf());
    assert!(matches!(
        lifecycle
            .claim_managed_attach(ClaimManagedAttach {
                raw_token: token,
                worker_id: worker.clone(),
                generation: generation.clone(),
                runtime_session_id: runtime_session_id.to_string(),
                tool: "claude".to_string(),
                cwd: cwd.to_string_lossy().into_owned(),
            })
            .unwrap(),
        ClaimOutcome::Active { .. }
    ));
    seed_entry(fanout.root(), runtime_session_id, cwd);
    fanout
        .attach_runtime(record_id, runtime_session_id)
        .unwrap();
    (worker, generation)
}

fn force_terminal_releasable_via_authority_edit_for_test(home: &Path, worker: &str) {
    let path = home.join("lifecycle-v1.json");
    let value: JsonValue = fs::read_to_string(&path).unwrap().parse().unwrap();
    let mut authority = value.get::<HashMap<String, JsonValue>>().unwrap().clone();
    let mut state = authority["state"]
        .get::<HashMap<String, JsonValue>>()
        .unwrap()
        .clone();
    let mut workers = state["managed_workers"]
        .get::<HashMap<String, JsonValue>>()
        .unwrap()
        .clone();
    let mut row = workers[worker]
        .get::<HashMap<String, JsonValue>>()
        .unwrap()
        .clone();
    row.insert(
        "state".into(),
        JsonValue::from("TerminalReleasable".to_string()),
    );
    workers.insert(worker.to_string(), JsonValue::from(row));
    state.insert("managed_workers".into(), JsonValue::from(workers));
    authority.insert("state".into(), JsonValue::from(state));
    fs::write(path, JsonValue::from(authority).format().unwrap()).unwrap();
}

fn force_collection_phase(home: &Path, reservation_id: &str, phase: &str) {
    let path = home.join("fanout-v1.json");
    let value: JsonValue = fs::read_to_string(&path).unwrap().parse().unwrap();
    let mut authority = value.get::<HashMap<String, JsonValue>>().unwrap().clone();
    let mut records = authority["records"]
        .get::<HashMap<String, JsonValue>>()
        .unwrap()
        .clone();
    let mut record = records[reservation_id]
        .get::<HashMap<String, JsonValue>>()
        .unwrap()
        .clone();
    record.insert("state".into(), JsonValue::from("Collecting".to_string()));
    record.insert(
        "collection_phase".into(),
        JsonValue::from(phase.to_string()),
    );
    records.insert(reservation_id.to_string(), JsonValue::from(record));
    authority.insert("records".into(), JsonValue::from(records));
    fs::write(path, JsonValue::from(authority).format().unwrap()).unwrap();
}

fn setup_root(tag: &str) -> (PathBuf, PathBuf, FanoutStore, fanout::FanoutRecord, String) {
    let home = fresh_home(tag);
    let repo = init_repo(&home);
    let invoker = "11111111-1111-4111-8111-111111111111";
    let root_session = "22222222-2222-4222-8222-222222222222";
    seed_entry(&home, invoker, &repo);
    let store = FanoutStore::new(home.clone());
    let root = fanout::prepare_worktree(&store, &repo, invoker, FanoutMode::Root).unwrap();
    activate_managed_fanout_worker(
        &store,
        &root.reservation_id,
        root_session,
        Path::new(&root.worktree),
    );
    (home, repo, store, root, root_session.to_string())
}

#[test]
fn authority_uses_separate_file_without_breaking_lifecycle_v1() {
    let (home, _repo, store, root, _root_session) = setup_root("separate-authority");
    assert!(home.join("fanout-v1.json").is_file());
    let lifecycle: JsonValue = fs::read_to_string(home.join("lifecycle-v1.json"))
        .unwrap()
        .parse()
        .unwrap();
    let lifecycle_state = lifecycle.get::<HashMap<String, JsonValue>>().unwrap()["state"]
        .get::<HashMap<String, JsonValue>>()
        .unwrap();
    assert!(!lifecycle_state.contains_key("fanout_records"));
    assert_eq!(
        store.read(&root.reservation_id).unwrap().unwrap().state,
        FanoutState::Running
    );
    fs::remove_dir_all(home).ok();
}

#[test]
fn authority_cap_is_atomic_and_child_ancestry_is_derived() {
    let (home, repo, store, root, root_session) = setup_root("atomic-cap");
    let first = fanout::prepare_worktree(&store, &repo, &root_session, FanoutMode::Child).unwrap();
    assert_eq!(first.depth, 1);
    assert_eq!(first.root_reservation_id, root.reservation_id);

    let store = Arc::new(store);
    let barrier = Arc::new(Barrier::new(3));
    let mut joins = Vec::new();
    for _ in 0..2 {
        let store = Arc::clone(&store);
        let barrier = Arc::clone(&barrier);
        let repo = repo.clone();
        let parent = root_session.clone();
        joins.push(thread::spawn(move || {
            barrier.wait();
            fanout::prepare_worktree(&store, &repo, &parent, FanoutMode::Child)
        }));
    }
    barrier.wait();
    let results = joins
        .into_iter()
        .map(|join| join.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    let error = results.into_iter().find_map(Result::err).unwrap();
    assert!(
        error.contains("fanout cap reached (2 active descendants)"),
        "{error}"
    );
    assert_eq!(store.active_leaf_count(&root.reservation_id).unwrap(), 2);
    assert_eq!(
        git(&repo, &["branch", "--list", "relay/fanout-*"])
            .lines()
            .count(),
        3,
        "root plus exactly two leaf branches"
    );
    fs::remove_dir_all(home).ok();
}

#[test]
fn authority_reserved_and_running_records_remain_counted_after_lifecycle_disagreement() {
    let (home, repo, store, root, root_session) = setup_root("cross-authority-cap");

    let reserved =
        fanout::prepare_worktree(&store, &repo, &root_session, FanoutMode::Child).unwrap();
    let (reserved_worker, _, _) = bind_pending_managed_fanout_worker(
        &store,
        &reserved.reservation_id,
        "33333333-3333-4333-8333-333333333333",
        Path::new(&reserved.worktree),
    );
    force_terminal_releasable_via_authority_edit_for_test(&home, &reserved_worker);
    assert_eq!(
        store.read(&reserved.reservation_id).unwrap().unwrap().state,
        FanoutState::Reserved
    );
    assert_eq!(store.active_leaf_count(&root.reservation_id).unwrap(), 1);

    let running =
        fanout::prepare_worktree(&store, &repo, &root_session, FanoutMode::Child).unwrap();
    let (running_worker, _) = activate_managed_fanout_worker(
        &store,
        &running.reservation_id,
        "44444444-4444-4444-8444-444444444444",
        Path::new(&running.worktree),
    );
    force_terminal_releasable_via_authority_edit_for_test(&home, &running_worker);
    assert_eq!(
        store.read(&running.reservation_id).unwrap().unwrap().state,
        FanoutState::Running
    );
    assert_eq!(store.active_leaf_count(&root.reservation_id).unwrap(), 2);

    let error =
        fanout::prepare_worktree(&store, &repo, &root_session, FanoutMode::Child).unwrap_err();
    assert!(
        error.contains("fanout cap reached (2 active descendants)"),
        "{error}"
    );
    fs::remove_dir_all(home).ok();
}

#[test]
fn authority_child_depth_and_root_are_not_caller_forgeable() {
    let (home, repo, store, root, _root_session) = setup_root("derived-ancestry");
    let invoker = "11111111-1111-4111-8111-111111111111";
    let branches_before = git(&repo, &["branch", "--list", "relay/fanout-*"]);
    assert_eq!(count_reservation_leaves(&home.join("worktrees")), 1);

    let error = fanout::prepare_worktree(&store, &repo, invoker, FanoutMode::Child).unwrap_err();

    assert!(error.contains("parent is not a managed root"), "{error}");
    assert_eq!(
        git(&repo, &["branch", "--list", "relay/fanout-*"]),
        branches_before
    );
    assert_eq!(count_reservation_leaves(&home.join("worktrees")), 1);
    assert_eq!(store.active_leaf_count(&root.reservation_id).unwrap(), 0);
    fs::remove_dir_all(home).ok();
}

#[test]
fn authority_managed_worker_cannot_create_a_nested_root() {
    let (home, repo, store, root, root_session) = setup_root("nested-root");
    let branches_before = git(&repo, &["branch", "--list", "relay/fanout-*"]);
    assert_eq!(count_reservation_leaves(&home.join("worktrees")), 1);

    let error =
        fanout::prepare_worktree(&store, &repo, &root_session, FanoutMode::Root).unwrap_err();

    assert!(error.contains("already a managed fanout worker"), "{error}");
    assert_eq!(
        git(&repo, &["branch", "--list", "relay/fanout-*"]),
        branches_before
    );
    assert_eq!(count_reservation_leaves(&home.join("worktrees")), 1);
    assert_eq!(store.active_leaf_count(&root.reservation_id).unwrap(), 0);
    fs::remove_dir_all(home).ok();
}

#[test]
fn authority_terminal_root_cannot_admit_a_new_leaf() {
    let (home, repo, store, root, root_session) = setup_root("terminal-root");
    let root_worker = store
        .read(&root.reservation_id)
        .unwrap()
        .unwrap()
        .worker_id
        .unwrap();
    force_terminal_releasable_via_authority_edit_for_test(&home, &root_worker);
    let branches_before = git(&repo, &["branch", "--list", "relay/fanout-*"]);
    assert_eq!(count_reservation_leaves(&home.join("worktrees")), 1);

    let error =
        fanout::prepare_worktree(&store, &repo, &root_session, FanoutMode::Child).unwrap_err();

    assert!(
        error.contains("not an exact Active managed root"),
        "{error}"
    );
    assert_eq!(
        git(&repo, &["branch", "--list", "relay/fanout-*"]),
        branches_before
    );
    assert_eq!(count_reservation_leaves(&home.join("worktrees")), 1);
    fs::remove_dir_all(home).ok();
}

#[test]
fn authority_read_only_fanout_is_rejected_before_reservation() {
    let home = fresh_home("read-only-refusal");
    let repo = init_repo(&home);
    let invoker = "91111111-1111-4111-8111-111111111111";
    seed_entry(&home, invoker, &repo);
    let output = Command::new(env!("CARGO_BIN_EXE_relay"))
        .args([
            "spawn",
            repo.to_str().unwrap(),
            "--fanout",
            "--from",
            invoker,
            "--read-only",
            "--tool",
            "claude",
            "--timeout",
            "1",
            "--",
            "cannot commit",
        ])
        .env("AGENT_RELAY_HOME", &home)
        .env("RELAY_SPAWN_CMD_CLAUDE", home.join("missing-claude"))
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("fanout spawn does not support --read-only")
    );
    assert!(!home.join("fanout-v1.json").exists());
    assert!(git(&repo, &["branch", "--list", "relay/fanout-*"]).is_empty());
    assert!(!home.join("worktrees").exists());
    fs::remove_dir_all(home).ok();
}

#[test]
fn authority_proven_no_launch_rollback_removes_only_the_pristine_worktree() {
    let (home, _repo, store, root, root_session) = setup_root("no-launch");
    let root_dir = PathBuf::from(&root.worktree);
    let missing_tool = home.join("missing-claude");
    let output = Command::new(env!("CARGO_BIN_EXE_relay"))
        .args([
            "spawn",
            root_dir.to_str().unwrap(),
            "--worktree",
            "--from",
            &root_session,
            "--tool",
            "claude",
            "--timeout",
            "2",
            "--",
            "never starts",
        ])
        .env("AGENT_RELAY_HOME", &home)
        .env("RELAY_SPAWN_CMD_CLAUDE", &missing_tool)
        .output()
        .unwrap();
    assert!(!output.status.success());

    let authority: JsonValue = fs::read_to_string(home.join("fanout-v1.json"))
        .unwrap()
        .parse()
        .unwrap();
    let records = authority.get::<HashMap<String, JsonValue>>().unwrap()["records"]
        .get::<HashMap<String, JsonValue>>()
        .unwrap();
    let failed = records
        .values()
        .filter_map(JsonValue::get::<HashMap<String, JsonValue>>)
        .find(|record| record["depth"].get::<String>().map(String::as_str) == Some("1"))
        .unwrap();
    assert_eq!(
        failed["state"].get::<String>().map(String::as_str),
        Some("FailedNoProcess")
    );
    let failed_worktree = Path::new(failed["worktree"].get::<String>().unwrap());
    assert_eq!(
        failed_worktree
            .strip_prefix(home.join("worktrees"))
            .unwrap()
            .components()
            .count(),
        2,
        "failed reservation must use the nested repository-key layout"
    );
    assert!(!failed_worktree.exists());
    assert_eq!(store.active_leaf_count(&root.reservation_id).unwrap(), 0);
    assert_eq!(
        git(&root_dir, &["branch", "--list", "relay/fanout-*"])
            .lines()
            .count(),
        2,
        "the root and failed leaf branches remain available for audit"
    );
    fs::remove_dir_all(home).ok();
}

#[test]
fn custody_exact_owned_process_reap_is_required_before_slot_release() {
    let (home, repo, store, root, root_session) = setup_root("custody-reap");
    let child = fanout::prepare_worktree(&store, &repo, &root_session, FanoutMode::Child).unwrap();
    let child_session = "53333333-3333-4333-8333-333333333333";
    let (worker_id, generation) = activate_managed_fanout_worker(
        &store,
        &child.reservation_id,
        child_session,
        Path::new(&child.worktree),
    );
    let lifecycle = LifecycleStore::new(home.clone());
    let custody = lifecycle
        .begin_owned_process_custody(
            &worker_id,
            &generation,
            &store::uuid_v4(),
            ProcessObservation {
                pid: std::process::id(),
                pgid: None,
                start: StartGeneration::Unavailable,
            },
        )
        .unwrap();
    let handback = fanout::handback(&store, child_session, "completed", "ready").unwrap();
    assert_eq!(handback.state, FanoutState::HandedBack);
    let fence = lifecycle
        .publish_fence(&worker_id, &generation, "fanout handback")
        .unwrap();
    let fenced = lifecycle
        .drain_prior_operations(fence)
        .unwrap()
        .confirm_process_terminal()
        .unwrap();

    let error = lifecycle
        .terminalize_worker(
            &worker_id,
            &generation,
            &fenced.version,
            TerminalAction::Release,
            "fanout process reaped",
        )
        .unwrap_err();
    assert!(
        error.contains("exact supervisor-owned process reap proof"),
        "{error}"
    );
    assert_eq!(store.active_leaf_count(&root.reservation_id).unwrap(), 1);

    lifecycle.record_owned_process_reaped(&custody, 0).unwrap();
    let released = lifecycle
        .terminalize_worker(
            &worker_id,
            &generation,
            &fenced.version,
            TerminalAction::Release,
            "fanout process reaped",
        )
        .unwrap();
    assert_eq!(released.state, ManagedState::TerminalReleasable);
    assert_eq!(store.active_leaf_count(&root.reservation_id).unwrap(), 0);
    fs::remove_dir_all(home).ok();
}

#[test]
fn custody_collection_waits_for_exact_owned_process_reap() {
    let (home, _repo, store, root, root_session) = setup_root("custody-collect");
    let root_dir = PathBuf::from(&root.worktree);
    let child =
        fanout::prepare_worktree(&store, &root_dir, &root_session, FanoutMode::Child).unwrap();
    let child_session = "73333333-3333-4333-8333-333333333333";
    let (worker_id, generation) = activate_managed_fanout_worker(
        &store,
        &child.reservation_id,
        child_session,
        Path::new(&child.worktree),
    );
    let lifecycle = LifecycleStore::new(home.clone());
    let custody = lifecycle
        .begin_owned_process_custody(
            &worker_id,
            &generation,
            &store::uuid_v4(),
            ProcessObservation {
                pid: std::process::id(),
                pgid: None,
                start: StartGeneration::Unavailable,
            },
        )
        .unwrap();

    fs::write(
        Path::new(&child.worktree).join("custody.txt"),
        "exact reap result\n",
    )
    .unwrap();
    git(Path::new(&child.worktree), &["add", "custody.txt"]);
    git(
        Path::new(&child.worktree),
        &["commit", "-qm", "exact reap result"],
    );
    let handback = fanout::handback(&store, child_session, "completed", "ready").unwrap();
    assert_eq!(handback.state, FanoutState::HandedBack);
    let parent_head_before = git(&root_dir, &["rev-parse", "HEAD"]);

    let collect_error = fanout::collect(&store, child_session, &root_session).unwrap_err();
    assert!(
        collect_error.contains("not TerminalReleasable"),
        "{collect_error}"
    );
    assert_eq!(
        store.read(&child.reservation_id).unwrap().unwrap().state,
        FanoutState::HandedBack
    );
    assert_eq!(store.active_leaf_count(&root.reservation_id).unwrap(), 1);
    assert!(Path::new(&child.worktree).is_dir());
    assert_eq!(git(&root_dir, &["rev-parse", "HEAD"]), parent_head_before);

    let fence = lifecycle
        .publish_fence(&worker_id, &generation, "fanout handback")
        .unwrap();
    let fenced = lifecycle
        .drain_prior_operations(fence)
        .unwrap()
        .confirm_process_terminal()
        .unwrap();
    let release_error = lifecycle
        .terminalize_worker(
            &worker_id,
            &generation,
            &fenced.version,
            TerminalAction::Release,
            "fanout process reaped",
        )
        .unwrap_err();
    assert!(
        release_error.contains("exact supervisor-owned process reap proof"),
        "{release_error}"
    );
    let collect_error = fanout::collect(&store, child_session, &root_session).unwrap_err();
    assert!(
        collect_error.contains("not TerminalReleasable"),
        "{collect_error}"
    );
    assert_eq!(
        store.read(&child.reservation_id).unwrap().unwrap().state,
        FanoutState::HandedBack
    );
    assert_eq!(store.active_leaf_count(&root.reservation_id).unwrap(), 1);
    assert!(Path::new(&child.worktree).is_dir());
    assert_eq!(git(&root_dir, &["rev-parse", "HEAD"]), parent_head_before);

    lifecycle.record_owned_process_reaped(&custody, 0).unwrap();
    let released = lifecycle
        .terminalize_worker(
            &worker_id,
            &generation,
            &fenced.version,
            TerminalAction::Release,
            "fanout process reaped",
        )
        .unwrap();
    assert_eq!(released.state, ManagedState::TerminalReleasable);

    let collected = fanout::collect(&store, child_session, &root_session).unwrap();
    assert_eq!(collected.state, FanoutState::Collected);
    assert!(root_dir.join("custody.txt").is_file());
    assert!(!Path::new(&child.worktree).exists());
    assert_ne!(git(&root_dir, &["rev-parse", "HEAD"]), parent_head_before);
    assert_eq!(store.active_leaf_count(&root.reservation_id).unwrap(), 0);
    fs::remove_dir_all(home).ok();
}

#[test]
fn custody_uncertain_process_state_keeps_the_slot_counted() {
    let (home, repo, store, root, root_session) = setup_root("custody-uncertain");
    let child = fanout::prepare_worktree(&store, &repo, &root_session, FanoutMode::Child).unwrap();
    let child_session = "63333333-3333-4333-8333-333333333333";
    let (worker_id, generation) = activate_managed_fanout_worker(
        &store,
        &child.reservation_id,
        child_session,
        Path::new(&child.worktree),
    );
    let lifecycle = LifecycleStore::new(home.clone());
    lifecycle
        .begin_owned_process_custody(
            &worker_id,
            &generation,
            &store::uuid_v4(),
            ProcessObservation {
                pid: std::process::id(),
                pgid: None,
                start: StartGeneration::Unavailable,
            },
        )
        .unwrap();
    let fence = lifecycle
        .publish_fence(&worker_id, &generation, "fanout handback")
        .unwrap();
    let fenced = lifecycle
        .drain_prior_operations(fence)
        .unwrap()
        .confirm_process_terminal()
        .unwrap();

    assert!(
        lifecycle
            .terminalize_worker(
                &worker_id,
                &generation,
                &fenced.version,
                TerminalAction::Release,
                "fanout process reaped",
            )
            .is_err()
    );
    assert_eq!(store.active_leaf_count(&root.reservation_id).unwrap(), 1);
    fs::remove_dir_all(home).ok();
}

#[test]
fn worktree_handback_and_collect_merge_once_then_remove_exact_tree() {
    let (home, _repo, store, root, root_session) = setup_root("collect");
    let root_dir = PathBuf::from(&root.worktree);
    let child =
        fanout::prepare_worktree(&store, &root_dir, &root_session, FanoutMode::Child).unwrap();
    let child_session = "33333333-3333-4333-8333-333333333333";
    let (worker, _) = activate_managed_fanout_worker(
        &store,
        &child.reservation_id,
        child_session,
        Path::new(&child.worktree),
    );
    fs::write(Path::new(&child.worktree).join("leaf.txt"), "leaf result\n").unwrap();
    git(Path::new(&child.worktree), &["add", "leaf.txt"]);
    git(
        Path::new(&child.worktree),
        &["commit", "-qm", "leaf result"],
    );
    let handback = fanout::handback(&store, child_session, "completed", "ready").unwrap();
    assert_eq!(handback.state, FanoutState::HandedBack);
    force_terminal_releasable_via_authority_edit_for_test(&home, &worker);

    let collected = fanout::collect(&store, child_session, &root_session).unwrap();
    assert_eq!(collected.state, FanoutState::Collected);
    assert!(root_dir.join("leaf.txt").is_file());
    assert!(!Path::new(&child.worktree).exists());
    let again = fanout::collect(&store, child_session, &root_session).unwrap();
    assert_eq!(again.state, FanoutState::Collected);
    assert_eq!(git(&root_dir, &["rev-list", "--count", "HEAD"]), "3");
    fs::remove_dir_all(home).ok();
}

#[test]
fn worktree_head_changed_after_handback_is_refused_without_removal() {
    let (home, _repo, store, root, root_session) = setup_root("head-changed");
    let root_dir = PathBuf::from(&root.worktree);
    let child =
        fanout::prepare_worktree(&store, &root_dir, &root_session, FanoutMode::Child).unwrap();
    let child_session = "93333333-3333-4333-8333-333333333333";
    let (worker, _) = activate_managed_fanout_worker(
        &store,
        &child.reservation_id,
        child_session,
        Path::new(&child.worktree),
    );
    fs::write(Path::new(&child.worktree).join("first.txt"), "first\n").unwrap();
    git(Path::new(&child.worktree), &["add", "first.txt"]);
    git(Path::new(&child.worktree), &["commit", "-qm", "first"]);
    let handback = fanout::handback(&store, child_session, "completed", "ready").unwrap();
    fs::write(Path::new(&child.worktree).join("late.txt"), "late\n").unwrap();
    git(Path::new(&child.worktree), &["add", "late.txt"]);
    git(Path::new(&child.worktree), &["commit", "-qm", "late"]);
    force_terminal_releasable_via_authority_edit_for_test(&home, &worker);

    let error = fanout::collect(&store, child_session, &root_session).unwrap_err();

    assert!(error.contains("changed after handback"), "{error}");
    assert_eq!(git(&root_dir, &["rev-parse", "HEAD"]), root.base_sha);
    assert!(Path::new(&child.worktree).exists());
    assert_eq!(
        store
            .read(&child.reservation_id)
            .unwrap()
            .unwrap()
            .handback_head,
        handback.handback_head
    );
    fs::remove_dir_all(home).ok();
}

#[test]
fn worktree_collection_lock_refuses_a_concurrent_collector_before_git_changes() {
    let (home, _repo, store, root, root_session) = setup_root("collect-lock");
    let root_dir = PathBuf::from(&root.worktree);
    let child =
        fanout::prepare_worktree(&store, &root_dir, &root_session, FanoutMode::Child).unwrap();
    let child_session = "a3333333-3333-4333-8333-333333333333";
    let (worker, _) = activate_managed_fanout_worker(
        &store,
        &child.reservation_id,
        child_session,
        Path::new(&child.worktree),
    );
    fs::write(Path::new(&child.worktree).join("leaf.txt"), "leaf\n").unwrap();
    git(Path::new(&child.worktree), &["add", "leaf.txt"]);
    git(Path::new(&child.worktree), &["commit", "-qm", "leaf"]);
    fanout::handback(&store, child_session, "completed", "ready").unwrap();
    force_terminal_releasable_via_authority_edit_for_test(&home, &worker);
    let lock_path = home
        .join("locks")
        .join(format!("fanout-collect-{}.lock", child.reservation_id));
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(lock_path)
        .unwrap();
    flock(&lock, FlockOperation::NonBlockingLockExclusive).unwrap();

    let error = fanout::collect(&store, child_session, &root_session).unwrap_err();

    assert!(error.contains("collection already in progress"), "{error}");
    assert_eq!(git(&root_dir, &["rev-parse", "HEAD"]), root.base_sha);
    assert!(Path::new(&child.worktree).exists());
    fs::remove_dir_all(home).ok();
}

#[test]
fn worktree_collect_recovers_when_removal_preceded_the_phase_write() {
    let (home, _repo, store, root, root_session) = setup_root("removed-before-phase");
    let root_dir = PathBuf::from(&root.worktree);
    let child =
        fanout::prepare_worktree(&store, &root_dir, &root_session, FanoutMode::Child).unwrap();
    let child_session = "b3333333-3333-4333-8333-333333333333";
    let (worker, _) = activate_managed_fanout_worker(
        &store,
        &child.reservation_id,
        child_session,
        Path::new(&child.worktree),
    );
    fs::write(Path::new(&child.worktree).join("leaf.txt"), "leaf\n").unwrap();
    git(Path::new(&child.worktree), &["add", "leaf.txt"]);
    git(Path::new(&child.worktree), &["commit", "-qm", "leaf"]);
    fanout::handback(&store, child_session, "completed", "ready").unwrap();
    force_terminal_releasable_via_authority_edit_for_test(&home, &worker);
    let head = store
        .read(&child.reservation_id)
        .unwrap()
        .unwrap()
        .handback_head
        .unwrap();
    git(&root_dir, &["merge", "--no-ff", "--no-edit", &head]);
    force_collection_phase(&home, &child.reservation_id, "Merged");
    git(&root_dir, &["worktree", "remove", child.worktree.as_str()]);

    let collected = fanout::collect(&store, child_session, &root_session).unwrap();

    assert_eq!(collected.state, FanoutState::Collected);
    assert!(root_dir.join("leaf.txt").is_file());
    assert!(!Path::new(&child.worktree).exists());
    fs::remove_dir_all(home).ok();
}

#[test]
fn worktree_dirty_handback_is_refused_without_publishing_merge_authority() {
    let (home, _repo, store, root, root_session) = setup_root("dirty-handback");
    let root_dir = PathBuf::from(&root.worktree);
    let child =
        fanout::prepare_worktree(&store, &root_dir, &root_session, FanoutMode::Child).unwrap();
    let child_session = "73333333-3333-4333-8333-333333333333";
    activate_managed_fanout_worker(
        &store,
        &child.reservation_id,
        child_session,
        Path::new(&child.worktree),
    );
    fs::write(
        Path::new(&child.worktree).join("uncommitted.txt"),
        "dirty\n",
    )
    .unwrap();

    let error = fanout::handback(&store, child_session, "completed", "not clean").unwrap_err();

    assert_eq!(error, "handback worktree is dirty");
    let current = store.read(&child.reservation_id).unwrap().unwrap();
    assert_eq!(current.state, FanoutState::Running);
    assert_eq!(current.handback_head, None);
    assert!(Path::new(&current.worktree).exists());
    fs::remove_dir_all(home).ok();
}

#[test]
fn worktree_dirty_parent_blocks_collection_until_the_same_checkout_is_clean() {
    let (home, _repo, store, root, root_session) = setup_root("dirty-collect");
    let root_dir = PathBuf::from(&root.worktree);
    let child =
        fanout::prepare_worktree(&store, &root_dir, &root_session, FanoutMode::Child).unwrap();
    let child_session = "83333333-3333-4333-8333-333333333333";
    let (worker, _) = activate_managed_fanout_worker(
        &store,
        &child.reservation_id,
        child_session,
        Path::new(&child.worktree),
    );
    fs::write(Path::new(&child.worktree).join("leaf.txt"), "leaf\n").unwrap();
    git(Path::new(&child.worktree), &["add", "leaf.txt"]);
    git(Path::new(&child.worktree), &["commit", "-qm", "leaf"]);
    fanout::handback(&store, child_session, "completed", "ready").unwrap();
    force_terminal_releasable_via_authority_edit_for_test(&home, &worker);
    let dirty = root_dir.join("uncommitted.txt");
    fs::write(&dirty, "dirty\n").unwrap();

    let error = fanout::collect(&store, child_session, &root_session).unwrap_err();

    assert_eq!(error, "collect parent is dirty");
    assert!(Path::new(&child.worktree).exists());
    assert!(!root_dir.join("leaf.txt").exists());
    assert_eq!(
        store.read(&child.reservation_id).unwrap().unwrap().state,
        FanoutState::Collecting
    );
    fs::remove_file(dirty).unwrap();
    assert_eq!(
        fanout::collect(&store, child_session, &root_session)
            .unwrap()
            .state,
        FanoutState::Collected
    );
    assert!(root_dir.join("leaf.txt").is_file());
    fs::remove_dir_all(home).ok();
}

#[test]
fn worktree_merge_conflict_aborts_cleanly_and_retries_after_parent_repair() {
    let (home, _repo, store, root, root_session) = setup_root("conflict");
    let root_dir = PathBuf::from(&root.worktree);
    let child =
        fanout::prepare_worktree(&store, &root_dir, &root_session, FanoutMode::Child).unwrap();
    let child_session = "43333333-3333-4333-8333-333333333333";
    let (worker, _) = activate_managed_fanout_worker(
        &store,
        &child.reservation_id,
        child_session,
        Path::new(&child.worktree),
    );
    fs::write(Path::new(&child.worktree).join("shared.txt"), "child\n").unwrap();
    git(Path::new(&child.worktree), &["add", "shared.txt"]);
    git(
        Path::new(&child.worktree),
        &["commit", "-qm", "child change"],
    );
    fanout::handback(&store, child_session, "completed", "conflicts").unwrap();
    force_terminal_releasable_via_authority_edit_for_test(&home, &worker);

    fs::write(root_dir.join("shared.txt"), "root\n").unwrap();
    git(&root_dir, &["add", "shared.txt"]);
    git(&root_dir, &["commit", "-qm", "root change"]);
    let error = fanout::collect(&store, child_session, &root_session).unwrap_err();
    assert!(error.contains("merge failed and was aborted"), "{error}");
    assert!(git(&root_dir, &["status", "--porcelain"]).is_empty());
    assert_eq!(
        store.read(&child.reservation_id).unwrap().unwrap().state,
        FanoutState::HandedBack
    );

    git(&root_dir, &["revert", "--no-edit", "HEAD"]);
    let collected = fanout::collect(&store, child_session, &root_session).unwrap();
    assert_eq!(collected.state, FanoutState::Collected);
    assert_eq!(
        fs::read_to_string(root_dir.join("shared.txt")).unwrap(),
        "child\n"
    );
    assert!(!Path::new(&child.worktree).exists());
    fs::remove_dir_all(home).ok();
}

#[test]
fn repository_gate_refuses_legacy_mode_before_fanout_mutation() {
    let home = fresh_home("repository-gate-mode");
    let repo = init_repo(&home);
    let opened = OpenedRepository::open(&repo).unwrap();
    let roots = SystemAuthorityRootProvider.roots().unwrap();
    let authority = WorkspaceAuthority::new(roots.clone()).unwrap();
    let gate = RepositoryGate::acquire(&roots, &opened.identity).unwrap();
    let repository_authority = authority
        .repository_dir(&opened.identity.repository_id)
        .unwrap();
    assert!(!repository_authority.exists());
    fs::create_dir(&repository_authority).unwrap();
    let error = gate
        .refuse_legacy_if_managed(&roots, &opened.identity)
        .unwrap_err();
    assert!(error.contains("managed workspace mode"), "{error}");
    assert!(git(&repo, &["branch", "--list", "relay/fanout-*"]).is_empty());
    assert!(!home.join("fanout-v1.json").exists());
    drop(gate);
    fs::remove_dir(&repository_authority).unwrap();
    fs::remove_dir_all(home).ok();
}

#[test]
fn repository_identity_accepts_reported_sha1_and_sha256_oid_widths() {
    for format in ["sha1", "sha256"] {
        let home = fresh_home(&format!("fanout-object-format-{format}"));
        let repo = home.join("repo");
        fs::create_dir_all(&repo).unwrap();
        let init = if format == "sha256" {
            Command::new("git")
                .args(["init", "-q", "--object-format=sha256"])
                .current_dir(&repo)
                .output()
                .unwrap()
        } else {
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(&repo)
                .output()
                .unwrap()
        };
        if format == "sha256" && !init.status.success() {
            fs::remove_dir_all(home).ok();
            continue;
        }
        assert!(init.status.success());
        git(&repo, &["config", "user.email", "relay@example.test"]);
        git(&repo, &["config", "user.name", "Relay Test"]);
        fs::write(repo.join("tracked"), "data\n").unwrap();
        git(&repo, &["add", "tracked"]);
        git(&repo, &["commit", "-qm", "base"]);
        let opened = OpenedRepository::open(&repo).unwrap();
        let head = opened.head().unwrap();
        assert_eq!(head.as_str().len(), if format == "sha1" { 40 } else { 64 });
        assert!(
            opened
                .validate_oid(&"a".repeat(if format == "sha1" { 64 } else { 40 }))
                .is_err()
        );
        fs::remove_dir_all(home).ok();
    }
}

struct CorrelatedRootFixture {
    home: PathBuf,
    fanout: FanoutStore,
    reservation: fanout::FanoutRecord,
    parent_session_id: String,
    runtime_session_id: String,
    worker_id: String,
    generation: String,
}

struct CorrelatedChildFixture {
    root: CorrelatedRootFixture,
    reservation: fanout::FanoutRecord,
    runtime_session_id: String,
    worker_id: String,
    generation: String,
}

fn setup_correlated_root(tag: &str) -> CorrelatedRootFixture {
    let home = fresh_home(tag);
    let repository = init_repo(&home);
    let parent_session_id = store::uuid_v4();
    let runtime_session_id = store::uuid_v4();
    seed_entry(&home, &parent_session_id, &repository);
    let fanout = FanoutStore::new(home.clone());
    let reserved =
        fanout::prepare_worktree(&fanout, &repository, &parent_session_id, FanoutMode::Root)
            .unwrap();
    seed_entry(&home, &runtime_session_id, Path::new(&reserved.worktree));
    let (worker_id, generation) = activate_managed_fanout_worker(
        &fanout,
        &reserved.reservation_id,
        &runtime_session_id,
        Path::new(&reserved.worktree),
    );
    let reservation = fanout
        .read(&reserved.reservation_id)
        .unwrap()
        .expect("attached root reservation");
    CorrelatedRootFixture {
        home,
        fanout,
        reservation,
        parent_session_id,
        runtime_session_id,
        worker_id,
        generation,
    }
}

fn setup_correlated_child(tag: &str) -> CorrelatedChildFixture {
    let root = setup_correlated_root(tag);
    let reserved = fanout::prepare_worktree(
        &root.fanout,
        Path::new(&root.reservation.worktree),
        &root.runtime_session_id,
        FanoutMode::Child,
    )
    .unwrap();
    let runtime_session_id = store::uuid_v4();
    seed_entry(
        &root.home,
        &runtime_session_id,
        Path::new(&reserved.worktree),
    );
    let (worker_id, generation) = activate_managed_fanout_worker(
        &root.fanout,
        &reserved.reservation_id,
        &runtime_session_id,
        Path::new(&reserved.worktree),
    );
    let reservation = root
        .fanout
        .read(&reserved.reservation_id)
        .unwrap()
        .expect("attached child reservation");
    CorrelatedChildFixture {
        root,
        reservation,
        runtime_session_id,
        worker_id,
        generation,
    }
}

fn commit_text_files(worktree: &Path, files: &[(&str, &str)], message: &str) -> String {
    for (relative, contents) in files {
        let path = worktree.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }
    git(worktree, &["add", "-A"]);
    git(worktree, &["commit", "-qm", message]);
    git(worktree, &["rev-parse", "HEAD"])
}

fn mailbox_bytes(home: &Path, session_id: &str) -> Vec<u8> {
    fs::read(home.join("mailbox").join(format!("{session_id}.jsonl"))).unwrap_or_default()
}

fn terminal_claim_path(home: &Path, correlation_id: &str) -> PathBuf {
    home.join("protocol-v1")
        .join("terminal")
        .join(format!("{correlation_id}.json"))
}

fn pending_claim_path(home: &Path, correlation_id: &str) -> PathBuf {
    home.join("protocol-v1")
        .join("pending")
        .join(format!("{correlation_id}.json"))
}

fn write_canonical_claim(path: &Path, bytes: &[u8]) {
    let mut file_bytes = bytes.to_vec();
    if !file_bytes.ends_with(b"\n") {
        file_bytes.push(b'\n');
    }
    fs::write(path, file_bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

fn fanout_record_object(home: &Path, reservation_id: &str) -> HashMap<String, JsonValue> {
    let authority: JsonValue = fs::read_to_string(home.join("fanout-v1.json"))
        .unwrap()
        .parse()
        .unwrap();
    authority.get::<HashMap<String, JsonValue>>().unwrap()["records"]
        .get::<HashMap<String, JsonValue>>()
        .unwrap()[reservation_id]
        .get::<HashMap<String, JsonValue>>()
        .unwrap()
        .clone()
}

fn replace_worker_result(home: &Path, reservation_id: &str, worker_result: &WorkerResultV1) {
    let encoded = String::from_utf8(worker_result.canonical_bytes()).unwrap();
    let value: JsonValue = encoded.parse().unwrap();
    mutate_fanout_record(home, reservation_id, |record| {
        record.insert("worker_result".into(), value);
        record.insert(
            "worker_result_sha256".into(),
            JsonValue::from(worker_result.sha256()),
        );
    });
}

fn mutate_terminal_claim(
    home: &Path,
    correlation_id: &str,
    mutate: impl FnOnce(&mut ClaimStatusV1),
) {
    let protocol = ProtocolStore::new(home.to_path_buf());
    let mut claim = protocol
        .read_claim(correlation_id)
        .unwrap()
        .expect("terminal claim");
    mutate(&mut claim);
    write_canonical_claim(
        &terminal_claim_path(home, correlation_id),
        &claim.canonical_bytes(),
    );
}

fn downgrade_reply_to_pending(
    home: &Path,
    correlation_id: &str,
    remove_mailbox_for: Option<&str>,
) -> Vec<u8> {
    let protocol = ProtocolStore::new(home.to_path_buf());
    let mut claim = protocol
        .read_claim(correlation_id)
        .unwrap()
        .expect("reply claim");
    let original = claim.canonical_bytes();
    claim.state = ClaimState::ReplyPending;
    claim.reply_delivery = Some(DeliveryState::Pending);
    let terminal = terminal_claim_path(home, correlation_id);
    let pending = pending_claim_path(home, correlation_id);
    fs::create_dir_all(pending.parent().unwrap()).unwrap();
    fs::rename(&terminal, &pending).unwrap();
    write_canonical_claim(&pending, &claim.canonical_bytes());
    if let Some(session_id) = remove_mailbox_for {
        fs::remove_file(home.join("mailbox").join(format!("{session_id}.jsonl"))).ok();
    }
    original
}

fn restore_terminal_claim(home: &Path, correlation_id: &str, bytes: &[u8]) {
    let pending = pending_claim_path(home, correlation_id);
    let terminal = terminal_claim_path(home, correlation_id);
    fs::rename(pending, &terminal).unwrap();
    write_canonical_claim(&terminal, bytes);
}

fn remove_protocol_claim(home: &Path, correlation_id: &str) {
    for directory in ["pending", "open", "terminal"] {
        fs::remove_file(
            home.join("protocol-v1")
                .join(directory)
                .join(format!("{correlation_id}.json")),
        )
        .ok();
    }
}

fn raw_fanout_state(home: &Path, reservation_id: &str) -> String {
    fanout_record_object(home, reservation_id)["state"]
        .get::<String>()
        .unwrap()
        .clone()
}

fn worker_result_notification_body(
    reservation_id: &str,
    worker_id: &str,
    generation: &str,
    runtime_session_id: &str,
    parent_session_id: &str,
) -> String {
    format!(
        "worker_result reservation_id={reservation_id} worker_id={worker_id} \
generation={generation} runtime_session_id={runtime_session_id} \
parent_session_id={parent_session_id}; collect: relay collect {runtime_session_id} \
--from {parent_session_id}"
    )
}

#[test]
fn correlated_reservation_and_attach_publish_exact_authority_request() {
    let root = setup_correlated_root("correlated-reservation");
    let reserved = fanout::prepare_worktree(
        &root.fanout,
        Path::new(&root.reservation.worktree),
        &root.runtime_session_id,
        FanoutMode::Child,
    )
    .unwrap();
    let correlation_id = reserved
        .correlation_id
        .clone()
        .expect("0.14 reservation correlation");
    assert!(store::is_uuid(&correlation_id));
    assert_eq!(reserved.request_message_id, None);
    assert_eq!(reserved.worker_result, None);
    assert_eq!(reserved.worker_result_sha256, None);

    let runtime_session_id = store::uuid_v4();
    seed_entry(
        &root.home,
        &runtime_session_id,
        Path::new(&reserved.worktree),
    );
    let (worker_id, generation) = activate_managed_fanout_worker(
        &root.fanout,
        &reserved.reservation_id,
        &runtime_session_id,
        Path::new(&reserved.worktree),
    );
    let attached = root
        .fanout
        .read(&reserved.reservation_id)
        .unwrap()
        .expect("attached reservation");
    assert_eq!(
        attached.correlation_id.as_deref(),
        Some(correlation_id.as_str())
    );
    assert_eq!(attached.worker_id.as_deref(), Some(worker_id.as_str()));
    assert_eq!(attached.generation.as_deref(), Some(generation.as_str()));
    assert_eq!(
        attached.runtime_session_id.as_deref(),
        Some(runtime_session_id.as_str())
    );

    let protocol = ProtocolStore::new(root.home.clone());
    let claim = protocol
        .read_claim(&correlation_id)
        .unwrap()
        .expect("attach-time fanout claim");
    assert_eq!(claim.schema, 1);
    assert_eq!(claim.correlation_id, correlation_id);
    assert_eq!(claim.origin, ClaimOrigin::Fanout);
    assert_eq!(claim.state, ClaimState::Open);
    assert_eq!(claim.requester_session_id, root.runtime_session_id);
    assert_eq!(claim.responder_session_id, runtime_session_id);
    assert_eq!(claim.request_delivery, DeliveryState::NotApplicable);
    assert_eq!(claim.reply, None);
    assert_eq!(claim.reply_sha256, None);
    assert_eq!(claim.reply_delivery, None);
    assert_eq!(claim.request.schema, 2);
    assert_eq!(claim.request.kind, MessageKind::Request);
    assert_eq!(claim.request.correlation_id, claim.correlation_id);
    assert_eq!(claim.request.from_session_id, claim.requester_session_id);
    assert_eq!(claim.request.to_session_id, claim.responder_session_id);
    assert_eq!(claim.request.reply_to, None);
    assert_eq!(claim.request.terminal_status, None);
    assert_eq!(claim.request.result_sha256, None);
    assert!(!claim.request.body.is_empty());
    assert_eq!(claim.request.sha256(), claim.request_sha256);
    assert_eq!(
        attached.request_message_id.as_deref(),
        Some(claim.request.id.as_str())
    );
    assert!(
        mailbox_bytes(&root.home, &runtime_session_id).is_empty(),
        "fanout authority requests are not physically mailbox-delivered"
    );
    fs::remove_dir_all(root.home).ok();
}

#[test]
fn handback_publishes_exact_completed_and_failed_worker_results_with_sorted_paths() {
    for (tag, status, expected_status) in [
        ("completed", "completed", TerminalStatus::Completed),
        ("failed", "failed", TerminalStatus::Failed),
    ] {
        let fixture = setup_correlated_child(&format!("worker-result-{tag}"));
        let worktree = Path::new(&fixture.reservation.worktree);
        let handback_commit = commit_text_files(
            worktree,
            &[
                ("zeta.txt", "zeta\n"),
                ("nested/middle.txt", "middle\n"),
                ("alpha.txt", "alpha\n"),
            ],
            &format!("{tag} result"),
        );
        let handed = fanout::handback(
            &fixture.root.fanout,
            &fixture.runtime_session_id,
            status,
            "summary",
        )
        .unwrap();
        let result = handed.worker_result.as_ref().expect("WorkerResultV1");
        assert_eq!(result.schema, 1);
        assert!(store::is_uuid(&result.result_id));
        assert_eq!(
            result.correlation_id,
            fixture.reservation.correlation_id.clone().unwrap()
        );
        assert_eq!(result.reservation_id, fixture.reservation.reservation_id);
        assert_eq!(
            result.root_reservation_id,
            fixture.reservation.root_reservation_id
        );
        assert_eq!(result.parent_session_id, fixture.root.runtime_session_id);
        assert_eq!(result.worker_id, fixture.worker_id);
        assert_eq!(result.generation, fixture.generation);
        assert_eq!(result.runtime_session_id, fixture.runtime_session_id);
        assert_eq!(result.repo_common_dir, fixture.reservation.repo_common_dir);
        assert_eq!(result.repo_dev, fixture.reservation.repo_dev);
        assert_eq!(result.repo_ino, fixture.reservation.repo_ino);
        assert_eq!(result.object_format, ObjectFormat::Sha1);
        assert_eq!(result.base_commit, fixture.reservation.base_sha);
        assert_eq!(result.handback_commit, handback_commit);
        assert_eq!(result.status, expected_status);
        assert_eq!(result.summary, "summary");
        assert_eq!(
            result.changed_paths,
            vec![
                "alpha.txt".to_string(),
                "nested/middle.txt".to_string(),
                "zeta.txt".to_string(),
            ]
        );
        assert!(result.canonical_bytes().len() <= 1024 * 1024);
        assert_eq!(
            handed.worker_result_sha256.as_deref(),
            Some(result.sha256().as_str())
        );

        let correlation_id = handed.correlation_id.as_deref().unwrap();
        let claim = ProtocolStore::new(fixture.root.home.clone())
            .read_claim(correlation_id)
            .unwrap()
            .expect("terminal fanout claim");
        assert_eq!(claim.state, ClaimState::ReplyEnqueued);
        assert_eq!(claim.reply_delivery, Some(DeliveryState::Enqueued));
        let reply = claim.reply.as_ref().expect("worker-result envelope");
        assert_eq!(reply.kind, MessageKind::WorkerResult);
        assert_eq!(reply.correlation_id, correlation_id);
        assert_eq!(
            reply.reply_to.as_deref(),
            handed.request_message_id.as_deref()
        );
        assert_eq!(reply.from_session_id, fixture.runtime_session_id);
        assert_eq!(reply.to_session_id, fixture.root.runtime_session_id);
        assert_eq!(reply.terminal_status.as_ref(), Some(&expected_status));
        assert_eq!(
            reply.result_sha256.as_deref(),
            handed.worker_result_sha256.as_deref()
        );
        assert_eq!(
            reply.body,
            worker_result_notification_body(
                &fixture.reservation.reservation_id,
                &fixture.worker_id,
                &fixture.generation,
                &fixture.runtime_session_id,
                &fixture.root.runtime_session_id,
            )
        );
        assert_eq!(claim.reply_sha256.as_deref(), Some(reply.sha256().as_str()));
        let mut expected_mailbox = reply.canonical_bytes();
        expected_mailbox.push(b'\n');
        assert_eq!(
            mailbox_bytes(&fixture.root.home, &fixture.root.runtime_session_id),
            expected_mailbox
        );

        if status == "failed" {
            let drained = ProtocolStore::new(fixture.root.home.clone())
                .drain_typed(&fixture.root.runtime_session_id)
                .unwrap();
            assert_eq!(drained.len(), 1);
            assert_eq!(drained[0].id, reply.id);
            let consumed = ProtocolStore::new(fixture.root.home.clone())
                .read_claim(correlation_id)
                .unwrap()
                .unwrap();
            assert_eq!(consumed.state, ClaimState::ReplyConsumed);
            assert_eq!(consumed.reply_delivery, Some(DeliveryState::Consumed));
        }

        force_terminal_releasable_via_authority_edit_for_test(
            &fixture.root.home,
            &fixture.worker_id,
        );
        let collected = fanout::collect(
            &fixture.root.fanout,
            &fixture.runtime_session_id,
            &fixture.root.runtime_session_id,
        )
        .unwrap();
        assert_eq!(collected.state, FanoutState::Collected);
        assert_eq!(collected.worker_result, handed.worker_result);
        assert_eq!(collected.worker_result_sha256, handed.worker_result_sha256);
        let parent = Path::new(&fixture.root.reservation.worktree);
        assert!(parent.join("alpha.txt").is_file());
        assert!(parent.join("nested/middle.txt").is_file());
        assert!(parent.join("zeta.txt").is_file());
        fs::remove_dir_all(fixture.root.home).ok();
    }
}

#[test]
fn duplicate_handback_is_exactly_idempotent_and_changed_retry_conflicts() {
    let fixture = setup_correlated_child("immutable-result");
    commit_text_files(
        Path::new(&fixture.reservation.worktree),
        &[("result.txt", "immutable\n")],
        "immutable result",
    );
    let first = fanout::handback(
        &fixture.root.fanout,
        &fixture.runtime_session_id,
        "completed",
        "same summary",
    )
    .unwrap();
    let correlation_id = first.correlation_id.as_deref().unwrap();
    let first_authority = fs::read(fixture.root.home.join("fanout-v1.json")).unwrap();
    let first_claim = ProtocolStore::new(fixture.root.home.clone())
        .read_claim(correlation_id)
        .unwrap()
        .unwrap()
        .canonical_bytes();
    let first_mailbox = mailbox_bytes(&fixture.root.home, &fixture.root.runtime_session_id);

    let retry = fanout::handback(
        &fixture.root.fanout,
        &fixture.runtime_session_id,
        "completed",
        "same summary",
    )
    .unwrap();
    assert_eq!(retry, first);
    assert_eq!(
        fs::read(fixture.root.home.join("fanout-v1.json")).unwrap(),
        first_authority
    );
    assert_eq!(
        ProtocolStore::new(fixture.root.home.clone())
            .read_claim(correlation_id)
            .unwrap()
            .unwrap()
            .canonical_bytes(),
        first_claim
    );
    assert_eq!(
        mailbox_bytes(&fixture.root.home, &fixture.root.runtime_session_id),
        first_mailbox
    );

    for (status, summary) in [("completed", "changed summary"), ("failed", "same summary")] {
        let error = fanout::handback(
            &fixture.root.fanout,
            &fixture.runtime_session_id,
            status,
            summary,
        )
        .unwrap_err();
        assert!(error.contains("correlation_conflict"), "{error}");
    }
    assert_eq!(
        fixture
            .root
            .fanout
            .read(&fixture.reservation.reservation_id)
            .unwrap()
            .unwrap(),
        first
    );
    assert_eq!(
        mailbox_bytes(&fixture.root.home, &fixture.root.runtime_session_id),
        first_mailbox
    );
    fs::remove_dir_all(fixture.root.home).ok();
}

#[test]
fn non_utf8_changed_path_is_refused_before_handback_authority_mutation() {
    let fixture = setup_correlated_child("non-utf8-result-path");
    let worktree = Path::new(&fixture.reservation.worktree);
    let invalid_name = OsString::from_vec(b"invalid-\xff-path".to_vec());
    fs::write(worktree.join(invalid_name), b"invalid path\n").unwrap();
    git(worktree, &["add", "-A"]);
    git(worktree, &["commit", "-qm", "non utf8 path"]);
    let before_record = fixture
        .root
        .fanout
        .read(&fixture.reservation.reservation_id)
        .unwrap()
        .unwrap();
    let before_authority = fs::read(fixture.root.home.join("fanout-v1.json")).unwrap();
    let correlation_id = before_record.correlation_id.as_deref().unwrap();
    let before_claim = ProtocolStore::new(fixture.root.home.clone())
        .read_claim(correlation_id)
        .unwrap()
        .unwrap()
        .canonical_bytes();

    let error = fanout::handback(
        &fixture.root.fanout,
        &fixture.runtime_session_id,
        "completed",
        "must refuse",
    )
    .unwrap_err();
    assert!(error.contains("UTF-8"), "{error}");
    assert_eq!(
        fixture
            .root
            .fanout
            .read(&fixture.reservation.reservation_id)
            .unwrap()
            .unwrap(),
        before_record
    );
    assert_eq!(
        fs::read(fixture.root.home.join("fanout-v1.json")).unwrap(),
        before_authority
    );
    assert_eq!(
        ProtocolStore::new(fixture.root.home.clone())
            .read_claim(correlation_id)
            .unwrap()
            .unwrap()
            .canonical_bytes(),
        before_claim
    );
    assert!(mailbox_bytes(&fixture.root.home, &fixture.root.runtime_session_id).is_empty());
    assert!(Path::new(&fixture.reservation.worktree).is_dir());
    fs::remove_dir_all(fixture.root.home).ok();
}

#[test]
fn collection_validates_every_repository_base_head_generation_and_runtime_binding() {
    for binding in ["repository", "base", "head", "generation", "runtime"] {
        let fixture = setup_correlated_child(&format!("result-binding-{binding}"));
        commit_text_files(
            Path::new(&fixture.reservation.worktree),
            &[("binding.txt", "binding\n")],
            "binding result",
        );
        let handed = fanout::handback(
            &fixture.root.fanout,
            &fixture.runtime_session_id,
            "completed",
            "binding",
        )
        .unwrap();
        let mut tampered = handed.worker_result.clone().unwrap();
        match binding {
            "repository" => tampered.repo_ino = "0".to_string(),
            "base" => tampered.base_commit = "e".repeat(40),
            "head" => tampered.handback_commit = "f".repeat(40),
            "generation" => tampered.generation = store::uuid_v4(),
            "runtime" => tampered.runtime_session_id = store::uuid_v4(),
            _ => unreachable!(),
        }
        replace_worker_result(
            &fixture.root.home,
            &fixture.reservation.reservation_id,
            &tampered,
        );
        let tampered_digest = tampered.sha256();
        let correlation_id = handed.correlation_id.as_deref().unwrap();
        mutate_terminal_claim(&fixture.root.home, correlation_id, |claim| {
            let reply = claim.reply.as_mut().unwrap();
            reply.result_sha256 = Some(tampered_digest);
            claim.reply_sha256 = Some(reply.sha256());
        });
        let claim = ProtocolStore::new(fixture.root.home.clone())
            .read_claim(correlation_id)
            .unwrap()
            .unwrap();
        let mut mailbox = claim.reply.unwrap().canonical_bytes();
        mailbox.push(b'\n');
        fs::write(
            fixture
                .root
                .home
                .join("mailbox")
                .join(format!("{}.jsonl", fixture.root.runtime_session_id)),
            mailbox,
        )
        .unwrap();
        force_terminal_releasable_via_authority_edit_for_test(
            &fixture.root.home,
            &fixture.worker_id,
        );
        let parent = Path::new(&fixture.root.reservation.worktree);
        let parent_head = git(parent, &["rev-parse", "HEAD"]);

        let error = fanout::collect(
            &fixture.root.fanout,
            &fixture.runtime_session_id,
            &fixture.root.runtime_session_id,
        )
        .unwrap_err();
        let lower = error.to_ascii_lowercase();
        let names_binding = match binding {
            "head" => lower.contains("head") || lower.contains("handback"),
            other => lower.contains(other),
        };
        assert!(
            names_binding || lower.contains("worker result binding"),
            "{error}"
        );
        assert_eq!(
            raw_fanout_state(&fixture.root.home, &fixture.reservation.reservation_id),
            "HandedBack"
        );
        assert_eq!(git(parent, &["rev-parse", "HEAD"]), parent_head);
        assert!(Path::new(&fixture.reservation.worktree).is_dir());
        fs::remove_dir_all(fixture.root.home).ok();
    }
}

#[test]
fn corrupt_result_never_self_heals_merges_or_releases_capacity() {
    for corruption in ["unknown-field", "digest"] {
        let fixture = setup_correlated_child(&format!("result-corruption-{corruption}"));
        commit_text_files(
            Path::new(&fixture.reservation.worktree),
            &[("corrupt.txt", "retain me\n")],
            "corrupt result",
        );
        fanout::handback(
            &fixture.root.fanout,
            &fixture.runtime_session_id,
            "completed",
            "corruption",
        )
        .unwrap();
        let second = fanout::prepare_worktree(
            &fixture.root.fanout,
            Path::new(&fixture.root.reservation.worktree),
            &fixture.root.runtime_session_id,
            FanoutMode::Child,
        )
        .unwrap();
        force_terminal_releasable_via_authority_edit_for_test(
            &fixture.root.home,
            &fixture.worker_id,
        );
        match corruption {
            "unknown-field" => mutate_fanout_record(
                &fixture.root.home,
                &fixture.reservation.reservation_id,
                |record| {
                    let mut result = record["worker_result"]
                        .get::<HashMap<String, JsonValue>>()
                        .unwrap()
                        .clone();
                    result.insert("forged".into(), JsonValue::from("field".to_string()));
                    record.insert("worker_result".into(), JsonValue::from(result));
                },
            ),
            "digest" => mutate_fanout_record(
                &fixture.root.home,
                &fixture.reservation.reservation_id,
                |record| {
                    record.insert(
                        "worker_result_sha256".into(),
                        JsonValue::from("0".repeat(64)),
                    );
                },
            ),
            _ => unreachable!(),
        }
        let corrupt_authority = fs::read(fixture.root.home.join("fanout-v1.json")).unwrap();
        let handback_error = fanout::handback(
            &fixture.root.fanout,
            &fixture.runtime_session_id,
            "completed",
            "corruption",
        )
        .unwrap_err();
        assert!(!handback_error.is_empty());
        assert_eq!(
            fs::read(fixture.root.home.join("fanout-v1.json")).unwrap(),
            corrupt_authority
        );
        let parent = Path::new(&fixture.root.reservation.worktree);
        let parent_head = git(parent, &["rev-parse", "HEAD"]);
        let collect_error = fanout::collect(
            &fixture.root.fanout,
            &fixture.runtime_session_id,
            &fixture.root.runtime_session_id,
        )
        .unwrap_err();
        assert!(!collect_error.is_empty());
        assert_eq!(
            raw_fanout_state(&fixture.root.home, &fixture.reservation.reservation_id),
            "HandedBack"
        );
        assert_eq!(git(parent, &["rev-parse", "HEAD"]), parent_head);
        assert!(Path::new(&fixture.reservation.worktree).is_dir());
        assert!(Path::new(&second.worktree).is_dir());

        let branches_before = git(parent, &["branch", "--list", "relay/fanout-*"]);
        assert_eq!(
            count_reservation_leaves(&fixture.root.home.join("worktrees")),
            3
        );
        let capacity_error = fanout::prepare_worktree(
            &fixture.root.fanout,
            parent,
            &fixture.root.runtime_session_id,
            FanoutMode::Child,
        )
        .unwrap_err();
        assert!(!capacity_error.is_empty());
        assert_eq!(
            git(parent, &["branch", "--list", "relay/fanout-*"]),
            branches_before
        );
        assert_eq!(
            count_reservation_leaves(&fixture.root.home.join("worktrees")),
            3
        );
    }
}

#[test]
fn supervisor_release_gate_requires_exact_terminal_result_delivery() {
    let fixture = setup_correlated_child("supervisor-result-gate");
    assert!(
        !fixture
            .root
            .fanout
            .result_delivery_ready(&fixture.reservation.reservation_id)
            .unwrap(),
        "a Running reservation has no terminal result delivery"
    );
    commit_text_files(
        Path::new(&fixture.reservation.worktree),
        &[("gate.txt", "gate\n")],
        "gate result",
    );
    let handed = fanout::handback(
        &fixture.root.fanout,
        &fixture.runtime_session_id,
        "completed",
        "gate",
    )
    .unwrap();
    let correlation_id = handed.correlation_id.as_deref().unwrap();
    assert!(
        fixture
            .root
            .fanout
            .result_delivery_ready(&fixture.reservation.reservation_id)
            .unwrap()
    );
    let terminal_bytes = downgrade_reply_to_pending(&fixture.root.home, correlation_id, None);
    assert!(
        !fixture
            .root
            .fanout
            .result_delivery_ready(&fixture.reservation.reservation_id)
            .unwrap(),
        "ReplyPending cannot release supervisor custody"
    );
    restore_terminal_claim(&fixture.root.home, correlation_id, &terminal_bytes);

    let exact_terminal = fs::read(terminal_claim_path(&fixture.root.home, correlation_id)).unwrap();
    fs::remove_file(terminal_claim_path(&fixture.root.home, correlation_id)).unwrap();
    assert!(
        !fixture
            .root
            .fanout
            .result_delivery_ready(&fixture.reservation.reservation_id)
            .unwrap(),
        "a missing terminal claim cannot release custody"
    );
    write_canonical_claim(
        &terminal_claim_path(&fixture.root.home, correlation_id),
        &exact_terminal,
    );
    mutate_terminal_claim(&fixture.root.home, correlation_id, |claim| {
        let reply = claim.reply.as_mut().unwrap();
        reply.result_sha256 = Some("0".repeat(64));
        claim.reply_sha256 = Some(reply.sha256());
    });
    assert!(
        !fixture
            .root
            .fanout
            .result_delivery_ready(&fixture.reservation.reservation_id)
            .unwrap(),
        "a valid terminal claim for another result digest cannot release custody"
    );
    write_canonical_claim(
        &terminal_claim_path(&fixture.root.home, correlation_id),
        &exact_terminal,
    );

    let exact_fanout = fs::read(fixture.root.home.join("fanout-v1.json")).unwrap();
    mutate_fanout_record(
        &fixture.root.home,
        &fixture.reservation.reservation_id,
        |record| {
            record.insert(
                "worker_result_sha256".into(),
                JsonValue::from("0".repeat(64)),
            );
        },
    );
    assert!(
        !fixture
            .root
            .fanout
            .result_delivery_ready(&fixture.reservation.reservation_id)
            .unwrap(),
        "corrupt embedded result authority cannot release custody"
    );
    fs::write(fixture.root.home.join("fanout-v1.json"), exact_fanout).unwrap();

    let reply_id = ProtocolStore::new(fixture.root.home.clone())
        .read_claim(correlation_id)
        .unwrap()
        .unwrap()
        .reply
        .unwrap()
        .id;
    let drained = ProtocolStore::new(fixture.root.home.clone())
        .drain_typed(&fixture.root.runtime_session_id)
        .unwrap();
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].id, reply_id);
    let consumed = ProtocolStore::new(fixture.root.home.clone())
        .read_claim(correlation_id)
        .unwrap()
        .unwrap();
    assert_eq!(consumed.state, ClaimState::ReplyConsumed);
    assert!(
        fixture
            .root
            .fanout
            .result_delivery_ready(&fixture.reservation.reservation_id)
            .unwrap(),
        "matching ReplyConsumed remains terminal delivery proof"
    );
    fs::remove_dir_all(fixture.root.home).ok();
}

#[test]
fn restart_replays_embedded_result_bytes_once_for_handback_and_collect() {
    let retry_fixture = setup_correlated_child("result-restart-handback");
    commit_text_files(
        Path::new(&retry_fixture.reservation.worktree),
        &[("retry.txt", "retry\n")],
        "retry result",
    );
    let first = fanout::handback(
        &retry_fixture.root.fanout,
        &retry_fixture.runtime_session_id,
        "completed",
        "retry",
    )
    .unwrap();
    let correlation_id = first.correlation_id.as_deref().unwrap();
    let expected_mailbox = mailbox_bytes(
        &retry_fixture.root.home,
        &retry_fixture.root.runtime_session_id,
    );
    downgrade_reply_to_pending(
        &retry_fixture.root.home,
        correlation_id,
        Some(&retry_fixture.root.runtime_session_id),
    );
    let restarted = FanoutStore::new(retry_fixture.root.home.clone());
    let recovered = fanout::handback(
        &restarted,
        &retry_fixture.runtime_session_id,
        "completed",
        "retry",
    )
    .unwrap();
    assert_eq!(recovered.worker_result, first.worker_result);
    assert_eq!(recovered.worker_result_sha256, first.worker_result_sha256);
    let recovered_claim = ProtocolStore::new(retry_fixture.root.home.clone())
        .read_claim(correlation_id)
        .unwrap()
        .unwrap();
    assert_eq!(recovered_claim.state, ClaimState::ReplyEnqueued);
    assert_eq!(
        mailbox_bytes(
            &retry_fixture.root.home,
            &retry_fixture.root.runtime_session_id
        ),
        expected_mailbox
    );
    fanout::handback(
        &restarted,
        &retry_fixture.runtime_session_id,
        "completed",
        "retry",
    )
    .unwrap();
    assert_eq!(
        mailbox_bytes(
            &retry_fixture.root.home,
            &retry_fixture.root.runtime_session_id
        ),
        expected_mailbox
    );
    fs::remove_dir_all(retry_fixture.root.home).ok();

    let collect_fixture = setup_correlated_child("result-restart-collect");
    commit_text_files(
        Path::new(&collect_fixture.reservation.worktree),
        &[("collect-retry.txt", "collect retry\n")],
        "collect retry result",
    );
    let handed = fanout::handback(
        &collect_fixture.root.fanout,
        &collect_fixture.runtime_session_id,
        "completed",
        "collect retry",
    )
    .unwrap();
    let correlation_id = handed.correlation_id.as_deref().unwrap();
    let expected_mailbox = mailbox_bytes(
        &collect_fixture.root.home,
        &collect_fixture.root.runtime_session_id,
    );
    downgrade_reply_to_pending(
        &collect_fixture.root.home,
        correlation_id,
        Some(&collect_fixture.root.runtime_session_id),
    );
    force_terminal_releasable_via_authority_edit_for_test(
        &collect_fixture.root.home,
        &collect_fixture.worker_id,
    );
    let restarted = FanoutStore::new(collect_fixture.root.home.clone());
    let collected = fanout::collect(
        &restarted,
        &collect_fixture.runtime_session_id,
        &collect_fixture.root.runtime_session_id,
    )
    .unwrap();
    assert_eq!(collected.state, FanoutState::Collected);
    assert_eq!(collected.worker_result, handed.worker_result);
    assert_eq!(
        ProtocolStore::new(collect_fixture.root.home.clone())
            .read_claim(correlation_id)
            .unwrap()
            .unwrap()
            .state,
        ClaimState::ReplyEnqueued
    );
    assert_eq!(
        mailbox_bytes(
            &collect_fixture.root.home,
            &collect_fixture.root.runtime_session_id
        ),
        expected_mailbox
    );
    assert!(
        Path::new(&collect_fixture.root.reservation.worktree)
            .join("collect-retry.txt")
            .is_file()
    );
    fs::remove_dir_all(collect_fixture.root.home).ok();
}

#[test]
fn collection_refuses_mismatched_terminal_claim_before_merge_and_retains_capacity() {
    let fixture = setup_correlated_child("claim-mismatch-custody");
    commit_text_files(
        Path::new(&fixture.reservation.worktree),
        &[("custody-result.txt", "custody\n")],
        "custody result",
    );
    let handed = fanout::handback(
        &fixture.root.fanout,
        &fixture.runtime_session_id,
        "completed",
        "custody",
    )
    .unwrap();
    let correlation_id = handed.correlation_id.as_deref().unwrap();
    mutate_terminal_claim(&fixture.root.home, correlation_id, |claim| {
        let reply = claim.reply.as_mut().unwrap();
        reply.result_sha256 = Some("0".repeat(64));
        claim.reply_sha256 = Some(reply.sha256());
    });
    assert!(
        !fixture
            .root
            .fanout
            .result_delivery_ready(&fixture.reservation.reservation_id)
            .unwrap()
    );
    force_terminal_releasable_via_authority_edit_for_test(&fixture.root.home, &fixture.worker_id);
    let parent = Path::new(&fixture.root.reservation.worktree);
    let parent_head = git(parent, &["rev-parse", "HEAD"]);
    let error = fanout::collect(
        &fixture.root.fanout,
        &fixture.runtime_session_id,
        &fixture.root.runtime_session_id,
    )
    .unwrap_err();
    assert!(!error.is_empty());
    let retained = fixture
        .root
        .fanout
        .read(&fixture.reservation.reservation_id)
        .unwrap()
        .unwrap();
    assert_eq!(retained.state, FanoutState::HandedBack);
    assert_eq!(retained.collection_phase, None);
    assert_eq!(git(parent, &["rev-parse", "HEAD"]), parent_head);
    assert!(Path::new(&fixture.reservation.worktree).is_dir());

    let second = fanout::prepare_worktree(
        &fixture.root.fanout,
        parent,
        &fixture.root.runtime_session_id,
        FanoutMode::Child,
    )
    .unwrap();
    assert!(Path::new(&second.worktree).is_dir());
    let third_error = fanout::prepare_worktree(
        &fixture.root.fanout,
        parent,
        &fixture.root.runtime_session_id,
        FanoutMode::Child,
    )
    .unwrap_err();
    assert!(third_error.contains("fanout cap reached"), "{third_error}");
    assert_eq!(
        fixture
            .root
            .fanout
            .active_leaf_count(&fixture.root.reservation.root_reservation_id)
            .unwrap(),
        2
    );
    fs::remove_dir_all(fixture.root.home).ok();
}

#[test]
fn pre_upgrade_record_without_result_fields_retains_legacy_handback_and_collect() {
    let fixture = setup_correlated_child("legacy-result-compatibility");
    let correlation_id = fixture.reservation.correlation_id.clone().unwrap();
    mutate_fanout_record(
        &fixture.root.home,
        &fixture.reservation.reservation_id,
        |record| {
            for key in [
                "correlation_id",
                "request_message_id",
                "worker_result",
                "worker_result_sha256",
            ] {
                record.remove(key);
            }
        },
    );
    remove_protocol_claim(&fixture.root.home, &correlation_id);
    let legacy = fixture
        .root
        .fanout
        .read(&fixture.reservation.reservation_id)
        .unwrap()
        .expect("pre-upgrade fanout record");
    assert_eq!(legacy.correlation_id, None);
    assert_eq!(legacy.request_message_id, None);
    assert_eq!(legacy.worker_result, None);
    assert_eq!(legacy.worker_result_sha256, None);
    commit_text_files(
        Path::new(&legacy.worktree),
        &[("legacy-result.txt", "legacy\n")],
        "legacy result",
    );
    let handed = fanout::handback(
        &fixture.root.fanout,
        &fixture.runtime_session_id,
        "completed",
        "legacy",
    )
    .unwrap();
    assert_eq!(handed.state, FanoutState::HandedBack);
    assert_eq!(handed.correlation_id, None);
    assert_eq!(handed.worker_result, None);
    assert_eq!(handed.worker_result_sha256, None);
    assert!(mailbox_bytes(&fixture.root.home, &fixture.root.runtime_session_id).is_empty());
    force_terminal_releasable_via_authority_edit_for_test(&fixture.root.home, &fixture.worker_id);
    let collected = fanout::collect(
        &fixture.root.fanout,
        &fixture.runtime_session_id,
        &fixture.root.runtime_session_id,
    )
    .unwrap();
    assert_eq!(collected.state, FanoutState::Collected);
    assert!(
        Path::new(&fixture.root.reservation.worktree)
            .join("legacy-result.txt")
            .is_file()
    );
    fs::remove_dir_all(fixture.root.home).ok();
}

#[test]
fn depth_zero_handback_waits_for_depth_one_collection_then_publishes_its_result() {
    let fixture = setup_correlated_child("depth-order-collected");
    let error = fanout::handback(
        &fixture.root.fanout,
        &fixture.root.runtime_session_id,
        "completed",
        "root too early",
    )
    .unwrap_err();
    assert_eq!(error, "fanout root has uncollected children");
    let unchanged_root = fixture
        .root
        .fanout
        .read(&fixture.root.reservation.reservation_id)
        .unwrap()
        .unwrap();
    assert_eq!(unchanged_root.state, FanoutState::Running);
    assert_eq!(unchanged_root.worker_result, None);

    commit_text_files(
        Path::new(&fixture.reservation.worktree),
        &[("ordered-child.txt", "ordered\n")],
        "ordered child",
    );
    fanout::handback(
        &fixture.root.fanout,
        &fixture.runtime_session_id,
        "completed",
        "child done",
    )
    .unwrap();
    force_terminal_releasable_via_authority_edit_for_test(&fixture.root.home, &fixture.worker_id);
    fanout::collect(
        &fixture.root.fanout,
        &fixture.runtime_session_id,
        &fixture.root.runtime_session_id,
    )
    .unwrap();

    let root_handback = fanout::handback(
        &fixture.root.fanout,
        &fixture.root.runtime_session_id,
        "completed",
        "root done",
    )
    .unwrap();
    let result = root_handback.worker_result.as_ref().unwrap();
    assert_eq!(result.worker_id, fixture.root.worker_id);
    assert_eq!(result.generation, fixture.root.generation);
    assert_eq!(result.runtime_session_id, fixture.root.runtime_session_id);
    assert_eq!(result.parent_session_id, fixture.root.parent_session_id);
    assert_eq!(result.changed_paths, vec!["ordered-child.txt".to_string()]);
    assert!(
        fixture
            .root
            .fanout
            .result_delivery_ready(&fixture.root.reservation.reservation_id)
            .unwrap()
    );
    fs::remove_dir_all(fixture.root.home).ok();
}

#[test]
fn depth_zero_handback_accepts_failed_no_process_child_without_fabricated_result() {
    let root = setup_correlated_root("depth-order-failed-no-process");
    let missing_tool = root.home.join("missing-claude");
    let output = Command::new(env!("CARGO_BIN_EXE_relay"))
        .args([
            "spawn",
            root.reservation.worktree.as_str(),
            "--worktree",
            "--from",
            &root.runtime_session_id,
            "--tool",
            "claude",
            "--timeout",
            "2",
            "--",
            "never starts",
        ])
        .env("AGENT_RELAY_HOME", &root.home)
        .env("RELAY_SPAWN_CMD_CLAUDE", &missing_tool)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let authority: JsonValue = fs::read_to_string(root.home.join("fanout-v1.json"))
        .unwrap()
        .parse()
        .unwrap();
    let records = authority.get::<HashMap<String, JsonValue>>().unwrap()["records"]
        .get::<HashMap<String, JsonValue>>()
        .unwrap();
    let failed = records
        .values()
        .filter_map(JsonValue::get::<HashMap<String, JsonValue>>)
        .find(|record| {
            record["depth"].get::<String>().map(String::as_str) == Some("1")
                && record["state"].get::<String>().map(String::as_str) == Some("FailedNoProcess")
        })
        .expect("failed-no-process child");
    assert!(
        failed
            .get("worker_result")
            .is_none_or(|value| value.get::<()>().is_some())
    );
    assert_eq!(
        root.fanout
            .active_leaf_count(&root.reservation.root_reservation_id)
            .unwrap(),
        0
    );

    let handed = fanout::handback(
        &root.fanout,
        &root.runtime_session_id,
        "completed",
        "root after failed launch",
    )
    .unwrap();
    assert!(handed.worker_result.is_some());
    assert!(
        root.fanout
            .result_delivery_ready(&root.reservation.reservation_id)
            .unwrap()
    );
    fs::remove_dir_all(root.home).ok();
}

#[test]
fn nested_record_round_trips_with_unchanged_serialized_key_set() {
    let home = fresh_home("nested-record-round-trip");
    let repo = init_repo(&home);
    let invoker = "a1111111-1111-4111-8111-111111111111";
    seed_entry(&home, invoker, &repo);
    let store = FanoutStore::new(home.clone());

    let reserved = fanout::prepare_worktree(&store, &repo, invoker, FanoutMode::Root).unwrap();
    let worktrees = home.join("worktrees");
    assert_eq!(
        Path::new(&reserved.worktree)
            .strip_prefix(&worktrees)
            .unwrap()
            .components()
            .count(),
        2,
        "a repository key must sit between worktrees and the reservation"
    );

    let authority: JsonValue = fs::read_to_string(home.join("fanout-v1.json"))
        .unwrap()
        .parse()
        .unwrap();
    let records = authority.get::<HashMap<String, JsonValue>>().unwrap()["records"]
        .get::<HashMap<String, JsonValue>>()
        .unwrap();
    let serialized = records[&reserved.reservation_id]
        .get::<HashMap<String, JsonValue>>()
        .unwrap();
    let mut serialized_keys: Vec<&str> = serialized.keys().map(String::as_str).collect();
    serialized_keys.sort_unstable();
    let mut expected_keys = [
        "base_sha",
        "branch",
        "collection_phase",
        "correlation_id",
        "depth",
        "generation",
        "handback_head",
        "handback_note",
        "handback_status",
        "last_error",
        "object_format",
        "parent_session_id",
        "repo_common_dir",
        "repo_dev",
        "repo_ino",
        "request_message_id",
        "reservation_id",
        "root_reservation_id",
        "runtime_session_id",
        "state",
        "version",
        "worker_id",
        "worker_result",
        "worker_result_sha256",
        "worktree",
    ];
    expected_keys.sort_unstable();
    assert_eq!(serialized_keys, expected_keys);
    assert_eq!(
        serialized["worktree"].get::<String>(),
        Some(&reserved.worktree)
    );

    let reparsed = store.read(&reserved.reservation_id).unwrap().unwrap();
    assert_eq!(reparsed.worktree, reserved.worktree);
    fs::remove_dir_all(home).ok();
}

#[test]
fn worktree_path_parser_rejects_each_invalid_shape_with_distinct_error() {
    let home = fresh_home("worktree-path-parser");
    let worktrees = home.join("worktrees");
    fs::create_dir_all(&worktrees).unwrap();
    let worktrees_dir = fs::File::open(&worktrees).unwrap();
    let reservation_id = "b1111111-1111-4111-8111-111111111111";
    let other_id = "b2222222-2222-4222-8222-222222222222";
    let cases = [
        (
            format!("/etc/{reservation_id}"),
            "fanout worktree path is outside the worktrees root",
        ),
        (
            format!("{}EVIL/{reservation_id}", worktrees.display()),
            "fanout worktree path is outside the worktrees root",
        ),
        (
            format!("{}/../{reservation_id}", worktrees.display()),
            "fanout worktree path has a relative component",
        ),
        (
            format!("{}/a//{reservation_id}", worktrees.display()),
            "fanout worktree path has an empty component",
        ),
        (
            format!("{}/A/{reservation_id}", worktrees.display()),
            "fanout worktree path component is not permitted",
        ),
        (
            format!("{}/a/b/c/{reservation_id}", worktrees.display()),
            "fanout worktree path depth is out of range",
        ),
        (
            format!("{}/key/{other_id}", worktrees.display()),
            "fanout worktree path leaf is not the reservation id",
        ),
    ];

    for (persisted, expected) in cases {
        let error = fanout::resolve_worktree_path(
            &worktrees,
            worktrees_dir.as_fd(),
            &persisted,
            reservation_id,
        )
        .unwrap_err();
        assert_eq!(error, expected, "persisted path: {persisted}");
    }
    fs::remove_dir_all(home).ok();
}

#[test]
fn repository_key_walk_refuses_intermediate_symlink() {
    let home = fresh_home("repo-key-intermediate-symlink");
    let repo = init_repo(&home);
    git(
        &repo,
        &[
            "remote",
            "add",
            "origin",
            "git@example.test:owner/repository.git",
        ],
    );
    let invoker = "c1111111-1111-4111-8111-111111111111";
    seed_entry(&home, invoker, &repo);
    let store = FanoutStore::new(home.clone());
    let outside = fresh_home("repo-key-symlink-target");
    let worktrees = home.join("worktrees");
    fs::create_dir_all(&worktrees).unwrap();
    symlink(&outside, worktrees.join("owner")).unwrap();

    let error = fanout::prepare_worktree(&store, &repo, invoker, FanoutMode::Root).unwrap_err();

    assert_eq!(
        error,
        "fanout worktree root component is a symlink or not a directory"
    );
    assert_eq!(count_reservation_leaves(&outside), 0);
    fs::remove_dir_all(home).ok();
    fs::remove_dir_all(outside).ok();
}

#[test]
fn rollback_retains_capacity_for_out_of_tree_record() {
    let home = fresh_home("rollback-out-of-tree");
    let repo = init_repo(&home);
    let invoker = "d1111111-1111-4111-8111-111111111111";
    seed_entry(&home, invoker, &repo);
    let store = FanoutStore::new(home.clone());
    let reserved = fanout::prepare_worktree(&store, &repo, invoker, FanoutMode::Root).unwrap();
    let outside = fresh_home("rollback-out-of-tree-cwd");
    mutate_fanout_record(&home, &reserved.reservation_id, |record| {
        record.insert(
            "worktree".into(),
            JsonValue::from(outside.to_string_lossy().into_owned()),
        );
    });

    let missing_command = home.join("missing-supervisor-command");
    let mut config = HashMap::new();
    config.insert(
        "reservation_id".into(),
        JsonValue::from(reserved.reservation_id.clone()),
    );
    config.insert(
        "cwd".into(),
        JsonValue::from(outside.to_string_lossy().into_owned()),
    );
    config.insert("tool".into(), JsonValue::from("claude".to_string()));
    config.insert(
        "command".into(),
        JsonValue::from(missing_command.to_string_lossy().into_owned()),
    );
    config.insert("arguments".into(), JsonValue::from(Vec::<JsonValue>::new()));
    config.insert("timeout_secs".into(), JsonValue::from(2.0));
    let encoded = JsonValue::from(config).stringify().unwrap();

    let mut supervisor = Command::new(env!("CARGO_BIN_EXE_relay"))
        .arg("__fanout-supervisor")
        .env("AGENT_RELAY_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    supervisor
        .stdin
        .take()
        .unwrap()
        .write_all(encoded.as_bytes())
        .unwrap();
    let output = supervisor.wait_with_output().unwrap();
    assert!(!output.status.success());
    let report: JsonValue = String::from_utf8(output.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let error = report.get::<HashMap<String, JsonValue>>().unwrap()["error"]
        .get::<String>()
        .unwrap();
    let (_, rollback_error) = error
        .rsplit_once("; rollback retained capacity: ")
        .expect("supervisor error must report retained rollback capacity");
    assert_eq!(
        rollback_error,
        "fanout worktree path is outside the worktrees root"
    );
    assert!(outside.is_dir());
    assert_eq!(
        store.read(&reserved.reservation_id).unwrap().unwrap().state,
        FanoutState::Reserved
    );
    fs::remove_dir_all(home).ok();
    fs::remove_dir_all(outside).ok();
}

#[test]
fn invalid_remote_component_falls_back_to_portable_digest_key() {
    let home = fresh_home("invalid-remote-digest-fallback");
    let repo = init_repo(&home);
    git(
        &repo,
        &[
            "remote",
            "add",
            "origin",
            "git@example.test:~user/repository.git",
        ],
    );
    let invoker = "e1111111-1111-4111-8111-111111111111";
    seed_entry(&home, invoker, &repo);
    let store = FanoutStore::new(home.clone());

    let reserved = fanout::prepare_worktree(&store, &repo, invoker, FanoutMode::Root).unwrap();

    let relative = Path::new(&reserved.worktree)
        .strip_prefix(home.join("worktrees"))
        .unwrap();
    let components: Vec<&str> = relative
        .components()
        .map(|component| component.as_os_str().to_str().unwrap())
        .collect();
    assert_eq!(
        components.last().copied(),
        Some(reserved.reservation_id.as_str())
    );
    let key_components = &components[..components.len() - 1];
    assert!(!key_components.is_empty());
    assert!(key_components.iter().all(|component| {
        !component.is_empty()
            && component.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'.' | b'-' | b'_')
            })
    }));
    assert_eq!(key_components.len(), 1);
    assert_eq!(key_components[0].len(), 64);
    assert!(
        key_components[0]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    );
    fs::remove_dir_all(home).ok();
}
