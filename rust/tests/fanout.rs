pub mod support;

use relay::fanout::{self, FanoutMode, FanoutState, FanoutStore};
use relay::lifecycle::{
    ClaimManagedAttach, ClaimOutcome, ExecutionBackend, LifecycleStore, ManagedState,
    PendingAttachSpec, ProcessObservation, RequiredScope, StartGeneration, TerminalAction,
};
use relay::store;
use relay::workspace::authority::{
    AuthorityRootProvider, SystemAuthorityRootProvider, WorkspaceAuthority,
};
use relay::workspace::git::OpenedRepository;
use relay::workspace::repository_gate::RepositoryGate;
use rustix::fs::{FlockOperation, flock};
use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Barrier};
use std::thread;
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
    let worktrees_before = fs::read_dir(home.join("worktrees")).unwrap().count();

    let error = fanout::prepare_worktree(&store, &repo, invoker, FanoutMode::Child).unwrap_err();

    assert!(error.contains("parent is not a managed root"), "{error}");
    assert_eq!(
        git(&repo, &["branch", "--list", "relay/fanout-*"]),
        branches_before
    );
    assert_eq!(
        fs::read_dir(home.join("worktrees")).unwrap().count(),
        worktrees_before
    );
    assert_eq!(store.active_leaf_count(&root.reservation_id).unwrap(), 0);
    fs::remove_dir_all(home).ok();
}

#[test]
fn authority_managed_worker_cannot_create_a_nested_root() {
    let (home, repo, store, root, root_session) = setup_root("nested-root");
    let branches_before = git(&repo, &["branch", "--list", "relay/fanout-*"]);
    let worktrees_before = fs::read_dir(home.join("worktrees")).unwrap().count();

    let error =
        fanout::prepare_worktree(&store, &repo, &root_session, FanoutMode::Root).unwrap_err();

    assert!(error.contains("already a managed fanout worker"), "{error}");
    assert_eq!(
        git(&repo, &["branch", "--list", "relay/fanout-*"]),
        branches_before
    );
    assert_eq!(
        fs::read_dir(home.join("worktrees")).unwrap().count(),
        worktrees_before
    );
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
    let worktrees_before = fs::read_dir(home.join("worktrees")).unwrap().count();

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
    assert_eq!(
        fs::read_dir(home.join("worktrees")).unwrap().count(),
        worktrees_before
    );
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
    assert!(!Path::new(failed["worktree"].get::<String>().unwrap()).exists());
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
    let repository_authority = authority.repository_dir(&opened.identity.repository_id).unwrap();
    assert!(!repository_authority.exists());
    fs::create_dir(&repository_authority).unwrap();
    let error = gate
        .refuse_legacy_if_managed(
            &roots,
            Path::new(&opened.identity.common_dir_realpath),
        )
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
        assert!(opened.validate_oid(&"a".repeat(if format == "sha1" { 64 } else { 40 })).is_err());
        fs::remove_dir_all(home).ok();
    }
}
