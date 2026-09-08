pub mod support;

use relay::lifecycle::{
    BindingState, ClaimManagedAttach, ClaimOutcome, ExecutionBackend, GcCheckpoint, GcControl,
    LifecycleStore, ManagedState, PendingAttachSpec, RequiredScope,
};
use std::fs;
use std::fs::FileTimes;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};
use support::fresh_home;
use tinyjson::JsonValue;

fn pending(
    worker_id: &str,
    generation: &str,
    expected_runtime_session_id: Option<&str>,
    tool: &str,
    cwd: &str,
    expires_at_ms: i64,
) -> PendingAttachSpec {
    PendingAttachSpec {
        worker_id: worker_id.to_string(),
        generation: generation.to_string(),
        expected_runtime_session_id: expected_runtime_session_id.map(str::to_string),
        expected_tool: tool.to_string(),
        expected_cwd: cwd.to_string(),
        expires_at_ms,
        required_scope: RequiredScope::ProcessOnly,
        execution: ExecutionBackend::SupervisorOwnedProcess,
    }
}

fn claim(
    token: &str,
    worker_id: &str,
    generation: &str,
    runtime_session_id: &str,
    tool: &str,
    cwd: &str,
) -> ClaimManagedAttach {
    ClaimManagedAttach {
        raw_token: token.to_string(),
        worker_id: worker_id.to_string(),
        generation: generation.to_string(),
        runtime_session_id: runtime_session_id.to_string(),
        tool: tool.to_string(),
        cwd: cwd.to_string(),
    }
}

fn assert_active(outcome: ClaimOutcome, duplicate: bool) {
    match outcome {
        ClaimOutcome::Active {
            duplicate: actual, ..
        } => assert_eq!(actual, duplicate),
        other => panic!("expected Active, got {other:?}"),
    }
}

fn wait_for_claiming(store: &LifecycleStore, session_id: &str) {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if matches!(
            store.read_binding(session_id).unwrap().map(|b| b.state),
            Some(BindingState::Claiming { .. })
        ) {
            return;
        }
        assert!(Instant::now() < deadline, "binding never entered Claiming");
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn managed_attach_claude_prebound_id_claims_active() {
    let home = fresh_home("claude");
    let store = LifecycleStore::new(home.clone());
    let worker = "11111111-1111-4111-8111-111111111111";
    let generation = "22222222-2222-4222-8222-222222222222";
    let session = "33333333-3333-4333-8333-333333333333";
    let cwd = home.join("project");
    fs::create_dir_all(&cwd).unwrap();

    store
        .create_pending(
            pending(
                worker,
                generation,
                Some(session),
                "claude",
                cwd.to_str().unwrap(),
                relay::store::now_ms() + 30_000,
            ),
            "claude-secret",
        )
        .unwrap();
    assert_active(
        store
            .claim_managed_attach(claim(
                "claude-secret",
                worker,
                generation,
                session,
                "claude",
                cwd.to_str().unwrap(),
            ))
            .unwrap(),
        false,
    );

    let binding = store.read_binding(session).unwrap().unwrap();
    assert_eq!(binding.binding_epoch, "1");
    assert!(matches!(binding.state, BindingState::Managed { .. }));
    let managed = store.read_worker(worker).unwrap().unwrap();
    assert_eq!(managed.state, ManagedState::Active);
    assert_eq!(managed.version, "2");
    assert_eq!(managed.runtime_session_id.as_deref(), Some(session));
    assert!(
        store
            .read_pending_by_token("claude-secret")
            .unwrap()
            .is_none()
    );
    assert!(
        !fs::read_to_string(home.join("registry.json"))
            .unwrap()
            .contains("claude-secret")
    );
    fs::remove_dir_all(home).ok();
}

#[test]
fn managed_attach_codex_claiming_invalidates_old_epoch_and_waits_for_drain() {
    let home = fresh_home("codex-drain");
    let store = Arc::new(LifecycleStore::with_timeouts(
        home.clone(),
        Duration::from_secs(2),
        Duration::from_secs(1),
    ));
    let worker = "41111111-1111-4111-8111-111111111111";
    let generation = "42222222-2222-4222-8222-222222222222";
    let session = "43333333-3333-4333-8333-333333333333";
    let cwd = home.join("project");
    fs::create_dir_all(&cwd).unwrap();
    store
        .create_pending(
            pending(
                worker,
                generation,
                None,
                "codex",
                cwd.to_str().unwrap(),
                relay::store::now_ms() + 30_000,
            ),
            "codex-secret",
        )
        .unwrap();
    let old_guard = store.hold_unmanaged_epoch(session).unwrap();
    assert_eq!(old_guard.binding_epoch(), "0");

    let claimant = {
        let store = Arc::clone(&store);
        let cwd = cwd.clone();
        thread::spawn(move || {
            store.claim_managed_attach(claim(
                "codex-secret",
                worker,
                generation,
                session,
                "codex",
                cwd.to_str().unwrap(),
            ))
        })
    };
    wait_for_claiming(&store, session);
    let claiming = store.read_binding(session).unwrap().unwrap();
    assert_eq!(claiming.binding_epoch, "1");
    assert!(store.hold_unmanaged_epoch(session).is_err());
    assert!(
        !claimant.is_finished(),
        "Active published before old guard drained"
    );
    drop(old_guard);
    assert_active(claimant.join().unwrap().unwrap(), false);

    let managed = store.read_worker(worker).unwrap().unwrap();
    assert_eq!(managed.state, ManagedState::Active);
    assert_eq!(managed.runtime_session_id.as_deref(), Some(session));
    fs::remove_dir_all(home).ok();
}

#[test]
fn managed_attach_concurrent_exact_duplicate_joins_one_claim() {
    let home = fresh_home("duplicate");
    let store = Arc::new(LifecycleStore::with_timeouts(
        home.clone(),
        Duration::from_secs(2),
        Duration::from_secs(1),
    ));
    let worker = "51111111-1111-4111-8111-111111111111";
    let generation = "52222222-2222-4222-8222-222222222222";
    let session = "53333333-3333-4333-8333-333333333333";
    let cwd = home.join("project");
    fs::create_dir_all(&cwd).unwrap();
    store
        .create_pending(
            pending(
                worker,
                generation,
                None,
                "codex",
                cwd.to_str().unwrap(),
                relay::store::now_ms() + 30_000,
            ),
            "duplicate-secret",
        )
        .unwrap();
    let guard = store.hold_unmanaged_epoch(session).unwrap();

    let run = |store: Arc<LifecycleStore>| {
        let cwd = cwd.clone();
        thread::spawn(move || {
            store.claim_managed_attach(claim(
                "duplicate-secret",
                worker,
                generation,
                session,
                "codex",
                cwd.to_str().unwrap(),
            ))
        })
    };
    let first = run(Arc::clone(&store));
    wait_for_claiming(&store, session);
    let second = run(Arc::clone(&store));
    drop(guard);
    let outcomes = [
        first.join().unwrap().unwrap(),
        second.join().unwrap().unwrap(),
    ];
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(
                outcome,
                ClaimOutcome::Active {
                    duplicate: false,
                    ..
                }
            ))
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(
                outcome,
                ClaimOutcome::Active {
                    duplicate: true,
                    ..
                }
            ))
            .count(),
        1
    );
    assert_eq!(
        store.read_binding(session).unwrap().unwrap().binding_epoch,
        "1"
    );
    assert_eq!(store.read_worker(worker).unwrap().unwrap().version, "2");
    fs::remove_dir_all(home).ok();
}

#[test]
fn managed_attach_resume_is_exact_and_does_not_churn_state() {
    let home = fresh_home("resume");
    let store = LifecycleStore::new(home.clone());
    let worker = "61111111-1111-4111-8111-111111111111";
    let generation = "62222222-2222-4222-8222-222222222222";
    let session = "63333333-3333-4333-8333-333333333333";
    let cwd = home.join("project");
    fs::create_dir_all(&cwd).unwrap();
    store
        .create_pending(
            pending(
                worker,
                generation,
                Some(session),
                "claude",
                cwd.to_str().unwrap(),
                relay::store::now_ms() + 30_000,
            ),
            "resume-secret",
        )
        .unwrap();
    assert_active(
        store
            .claim_managed_attach(claim(
                "resume-secret",
                worker,
                generation,
                session,
                "claude",
                cwd.to_str().unwrap(),
            ))
            .unwrap(),
        false,
    );
    assert_active(
        store
            .resume_managed_attach(session, "claude", cwd.to_str().unwrap())
            .unwrap(),
        true,
    );
    assert_eq!(store.read_worker(worker).unwrap().unwrap().version, "2");
    assert!(matches!(
        store.resume_managed_attach(session, "codex", cwd.to_str().unwrap()),
        Ok(ClaimOutcome::Refused { .. })
    ));
    assert!(matches!(
        store.resume_managed_attach(session, "claude", "/wrong/cwd"),
        Ok(ClaimOutcome::Refused { .. })
    ));
    fs::remove_dir_all(home).ok();
}

#[test]
fn managed_attach_replay_and_tuple_mismatches_fail_without_rebinding() {
    let home = fresh_home("mismatch");
    let store = LifecycleStore::new(home.clone());
    let worker = "71111111-1111-4111-8111-111111111111";
    let generation = "72222222-2222-4222-8222-222222222222";
    let session = "73333333-3333-4333-8333-333333333333";
    let other = "74444444-4444-4444-8444-444444444444";
    let cwd = home.join("project");
    fs::create_dir_all(&cwd).unwrap();
    store
        .create_pending(
            pending(
                worker,
                generation,
                Some(session),
                "claude",
                cwd.to_str().unwrap(),
                relay::store::now_ms() + 30_000,
            ),
            "tuple-secret",
        )
        .unwrap();

    for bad in [
        claim(
            "tuple-secret",
            worker,
            generation,
            other,
            "claude",
            cwd.to_str().unwrap(),
        ),
        claim(
            "tuple-secret",
            worker,
            generation,
            session,
            "codex",
            cwd.to_str().unwrap(),
        ),
        claim(
            "tuple-secret",
            worker,
            generation,
            session,
            "claude",
            "/wrong/cwd",
        ),
        claim(
            "tuple-secret",
            worker,
            "75555555-5555-4555-8555-555555555555",
            session,
            "claude",
            cwd.to_str().unwrap(),
        ),
    ] {
        assert!(matches!(
            store.claim_managed_attach(bad),
            Ok(ClaimOutcome::Refused { .. })
        ));
    }
    assert!(store.read_binding(session).unwrap().is_none());
    assert_eq!(
        store.read_worker(worker).unwrap().unwrap().state,
        ManagedState::Attaching
    );

    assert_active(
        store
            .claim_managed_attach(claim(
                "tuple-secret",
                worker,
                generation,
                session,
                "claude",
                cwd.to_str().unwrap(),
            ))
            .unwrap(),
        false,
    );
    assert!(matches!(
        store.claim_managed_attach(claim(
            "tuple-secret",
            worker,
            generation,
            other,
            "claude",
            cwd.to_str().unwrap(),
        )),
        Ok(ClaimOutcome::Refused { .. })
    ));
    assert!(store.read_binding(other).unwrap().is_none());
    fs::remove_dir_all(home).ok();
}

#[test]
fn managed_attach_expiry_fences_worker_without_binding_session() {
    let home = fresh_home("expiry");
    let store = LifecycleStore::new(home.clone());
    let worker = "81111111-1111-4111-8111-111111111111";
    let generation = "82222222-2222-4222-8222-222222222222";
    let session = "83333333-3333-4333-8333-333333333333";
    store
        .create_pending(
            pending(
                worker,
                generation,
                None,
                "codex",
                home.to_str().unwrap(),
                relay::store::now_ms() - 1,
            ),
            "expired-secret",
        )
        .unwrap();
    assert!(matches!(
        store.claim_managed_attach(claim(
            "expired-secret",
            worker,
            generation,
            session,
            "codex",
            home.to_str().unwrap(),
        )),
        Ok(ClaimOutcome::Refused {
            state: ManagedState::Fencing,
            ..
        })
    ));
    assert!(store.read_binding(session).unwrap().is_none());
    assert_eq!(
        store.read_worker(worker).unwrap().unwrap().state,
        ManagedState::Fencing
    );
    fs::remove_dir_all(home).ok();
}

#[test]
fn managed_attach_conflicting_binding_refuses_second_worker() {
    let home = fresh_home("conflict");
    let store = LifecycleStore::new(home.clone());
    let session = "93333333-3333-4333-8333-333333333333";
    let cwd = home.join("project");
    fs::create_dir_all(&cwd).unwrap();
    let first = (
        "91111111-1111-4111-8111-111111111111",
        "92222222-2222-4222-8222-222222222222",
        "first-secret",
    );
    let second = (
        "94444444-4444-4444-8444-444444444444",
        "95555555-5555-4555-8555-555555555555",
        "second-secret",
    );
    for (worker, generation, token) in [first, second] {
        store
            .create_pending(
                pending(
                    worker,
                    generation,
                    Some(session),
                    "claude",
                    cwd.to_str().unwrap(),
                    relay::store::now_ms() + 30_000,
                ),
                token,
            )
            .unwrap();
    }
    assert_active(
        store
            .claim_managed_attach(claim(
                first.2,
                first.0,
                first.1,
                session,
                "claude",
                cwd.to_str().unwrap(),
            ))
            .unwrap(),
        false,
    );
    assert!(matches!(
        store.claim_managed_attach(claim(
            second.2,
            second.0,
            second.1,
            session,
            "claude",
            cwd.to_str().unwrap(),
        )),
        Ok(ClaimOutcome::Refused { .. })
    ));
    assert_eq!(
        store.read_worker(second.0).unwrap().unwrap().state,
        ManagedState::Attaching
    );
    fs::remove_dir_all(home).ok();
}

#[test]
fn managed_attach_drain_timeout_binds_fencing_never_active() {
    let home = fresh_home("timeout");
    let store = LifecycleStore::with_timeouts(
        home.clone(),
        Duration::from_millis(500),
        Duration::from_millis(50),
    );
    let worker = "a1111111-1111-4111-8111-111111111111";
    let generation = "a2222222-2222-4222-8222-222222222222";
    let session = "a3333333-3333-4333-8333-333333333333";
    store
        .create_pending(
            pending(
                worker,
                generation,
                None,
                "codex",
                home.to_str().unwrap(),
                relay::store::now_ms() + 30_000,
            ),
            "timeout-secret",
        )
        .unwrap();
    let guard = store.hold_unmanaged_epoch(session).unwrap();
    let outcome = store
        .claim_managed_attach(claim(
            "timeout-secret",
            worker,
            generation,
            session,
            "codex",
            home.to_str().unwrap(),
        ))
        .unwrap();
    assert!(matches!(
        outcome,
        ClaimOutcome::Refused {
            state: ManagedState::FencingUnconfirmed,
            ..
        }
    ));
    assert!(matches!(
        store.read_binding(session).unwrap().unwrap().state,
        BindingState::Managed { .. }
    ));
    let fenced = store.read_worker(worker).unwrap().unwrap();
    assert_eq!(fenced.state, ManagedState::FencingUnconfirmed);
    let operation_id = fenced
        .proof_gap
        .as_deref()
        .and_then(|gap| gap.split_once("active_operations="))
        .map(|(_, ids)| ids)
        .expect("claim timeout must persist its active operation id");
    assert!(relay::store::is_uuid(operation_id));
    drop(guard);
    fs::remove_dir_all(home).ok();
}

#[test]
fn managed_attach_closed_transition_graph_and_exact_versions() {
    let home = fresh_home("transitions");
    let store = LifecycleStore::new(home.clone());
    let worker = "b1111111-1111-4111-8111-111111111111";
    let generation = "b2222222-2222-4222-8222-222222222222";
    store
        .create_pending(
            pending(
                worker,
                generation,
                None,
                "codex",
                home.to_str().unwrap(),
                relay::store::now_ms() + 30_000,
            ),
            "transition-secret",
        )
        .unwrap();
    assert!(
        store
            .transition_worker(worker, generation, "1", ManagedState::Fenced)
            .is_err()
    );
    let fencing = store
        .transition_worker(worker, generation, "1", ManagedState::Fencing)
        .unwrap();
    assert_eq!(fencing.version, "2");
    assert!(
        store
            .transition_worker(worker, generation, "1", ManagedState::FencingUnconfirmed)
            .is_err()
    );
    let unconfirmed = store
        .transition_worker(worker, generation, "2", ManagedState::FencingUnconfirmed)
        .unwrap();
    assert_eq!(unconfirmed.version, "3");
    let retry = store
        .transition_worker(worker, generation, "3", ManagedState::Fencing)
        .unwrap();
    assert_eq!(retry.version, "4");
    assert!(
        store
            .transition_worker(worker, "wrong-generation", "4", ManagedState::Fenced)
            .is_err()
    );
    fs::remove_dir_all(home).ok();
}

fn json_object(value: &JsonValue) -> &std::collections::HashMap<String, JsonValue> {
    value
        .get::<std::collections::HashMap<String, JsonValue>>()
        .expect("JSON object")
}

fn registry(home: &Path) -> JsonValue {
    fs::read_to_string(home.join("registry.json"))
        .unwrap()
        .parse()
        .unwrap()
}

fn write_registry(home: &Path, value: JsonValue) {
    fs::write(home.join("registry.json"), value.format().unwrap()).unwrap();
}

fn lifecycle_authority(home: &Path) -> JsonValue {
    fs::read_to_string(home.join("lifecycle-v1.json"))
        .unwrap()
        .parse()
        .unwrap()
}

fn write_lifecycle_authority(home: &Path, value: JsonValue) {
    fs::write(home.join("lifecycle-v1.json"), value.format().unwrap()).unwrap();
}

fn seed_old_unmanaged(store: &LifecycleStore, home: &Path, id: &str) {
    let guard = store.hold_unmanaged_epoch(id).unwrap();
    drop(guard);
    let mut root = registry(home)
        .get::<std::collections::HashMap<String, JsonValue>>()
        .unwrap()
        .clone();
    let mut agents = root["agents"]
        .get::<std::collections::HashMap<String, JsonValue>>()
        .unwrap()
        .clone();
    let mut entry = std::collections::HashMap::new();
    entry.insert("id".into(), JsonValue::from(id.to_string()));
    entry.insert(
        "dir".into(),
        JsonValue::from(home.to_string_lossy().into_owned()),
    );
    entry.insert("name".into(), JsonValue::from(()));
    entry.insert("tool".into(), JsonValue::from("claude".to_string()));
    entry.insert(
        "lastSeen".into(),
        JsonValue::from("2020-01-01T00:00:00.000Z".to_string()),
    );
    entry.insert("server".into(), JsonValue::from(()));
    entry.insert("spawned_via".into(), JsonValue::from(()));
    agents.insert(id.to_string(), JsonValue::from(entry));
    root.insert("agents".into(), JsonValue::from(agents));
    write_registry(home, JsonValue::from(root));

    let mailbox = home.join("mailbox").join(format!("{id}.jsonl"));
    let marker = home
        .join("markers")
        .join(relay::store::encode_dir(home.to_str().unwrap()));
    fs::create_dir_all(mailbox.parent().unwrap()).unwrap();
    fs::create_dir_all(marker.parent().unwrap()).unwrap();
    fs::write(&mailbox, "{\"old\":true}\n").unwrap();
    fs::write(&marker, format!("{id}\n")).unwrap();
    let old = SystemTime::now() - Duration::from_secs(40 * 24 * 60 * 60);
    let times = FileTimes::new().set_modified(old).set_accessed(old);
    for path in [
        mailbox,
        marker,
        home.join("locks").join(format!("binding-{id}.lock")),
    ] {
        fs::File::options()
            .read(true)
            .write(true)
            .open(path)
            .unwrap()
            .set_times(times)
            .unwrap();
    }
}

fn registry_has_binding(home: &Path, id: &str) -> bool {
    let authority = lifecycle_authority(home);
    let state = json_object(&authority)["state"]
        .get::<std::collections::HashMap<String, JsonValue>>()
        .unwrap();
    state["session_bindings"]
        .get::<std::collections::HashMap<String, JsonValue>>()
        .unwrap()
        .contains_key(id)
}

#[test]
fn managed_gc_non_releasable_workers_pending_and_managed_bindings_survive_age() {
    let home = fresh_home("gc-retain");
    let store = LifecycleStore::new(home.clone());
    let states = [
        ManagedState::Attaching,
        ManagedState::Active,
        ManagedState::Fencing,
        ManagedState::FencingUnconfirmed,
        ManagedState::Fenced,
        ManagedState::TerminalRetained,
    ];
    for (index, target) in states.into_iter().enumerate() {
        let worker = format!("c{index}111111-1111-4111-8111-111111111111");
        let generation = format!("c{index}222222-2222-4222-8222-222222222222");
        let session = format!("c{index}333333-3333-4333-8333-333333333333");
        let token = format!("retain-{index}");
        store
            .create_pending(
                pending(
                    &worker,
                    &generation,
                    Some(&session),
                    "claude",
                    home.to_str().unwrap(),
                    relay::store::now_ms() + 30_000,
                ),
                &token,
            )
            .unwrap();
        if target != ManagedState::Attaching {
            assert_active(
                store
                    .claim_managed_attach(claim(
                        &token,
                        &worker,
                        &generation,
                        &session,
                        "claude",
                        home.to_str().unwrap(),
                    ))
                    .unwrap(),
                false,
            );
            let mut version = "2".to_string();
            let path: &[ManagedState] = match target {
                ManagedState::Active => &[],
                ManagedState::Fencing => &[ManagedState::Fencing],
                ManagedState::FencingUnconfirmed => {
                    &[ManagedState::Fencing, ManagedState::FencingUnconfirmed]
                }
                ManagedState::Fenced => &[ManagedState::Fencing, ManagedState::Fenced],
                ManagedState::TerminalRetained => &[
                    ManagedState::Fencing,
                    ManagedState::Fenced,
                    ManagedState::TerminalRetained,
                ],
                _ => unreachable!(),
            };
            for state in path {
                version = store
                    .transition_worker(&worker, &generation, &version, *state)
                    .unwrap()
                    .version;
            }
        }
    }

    let before = fs::read(home.join("registry.json")).unwrap();
    let result = store
        .gc_unmanaged(SystemTime::now(), GcControl::RunToCompletion)
        .unwrap();
    assert_eq!(result.removed_candidates, 0);
    assert_eq!(fs::read(home.join("registry.json")).unwrap(), before);
    fs::remove_dir_all(home).ok();
}

#[test]
fn managed_gc_old_standalone_unmanaged_uses_gcdel_cas_and_removes_record_last() {
    let home = fresh_home("gc-old");
    let store = LifecycleStore::new(home.clone());
    let id = "d3333333-3333-4333-8333-333333333333";
    seed_old_unmanaged(&store, &home, id);

    let result = store
        .gc_unmanaged(SystemTime::now(), GcControl::RunToCompletion)
        .unwrap();
    assert_eq!(result.removed_candidates, 1);
    assert!(!registry_has_binding(&home, id));
    assert!(
        !json_object(&registry(&home))["agents"]
            .get::<std::collections::HashMap<String, JsonValue>>()
            .unwrap()
            .contains_key(id)
    );
    assert!(!home.join("mailbox").join(format!("{id}.jsonl")).exists());
    assert!(
        !home
            .join("locks")
            .join(format!("binding-{id}.lock"))
            .exists()
    );
    fs::remove_dir_all(home).ok();
}

#[test]
fn managed_gc_live_lock_and_malformed_binding_are_fail_closed() {
    let home = fresh_home("gc-closed");
    let store = LifecycleStore::new(home.clone());
    let live = "e3333333-3333-4333-8333-333333333333";
    seed_old_unmanaged(&store, &home, live);
    let guard = store.hold_unmanaged_epoch(live).unwrap();
    assert_eq!(
        store
            .gc_unmanaged(SystemTime::now(), GcControl::RunToCompletion)
            .unwrap()
            .removed_candidates,
        0
    );
    drop(guard);

    let malformed = "e4444444-4444-4444-8444-444444444444";
    seed_old_unmanaged(&store, &home, malformed);
    let mut root = lifecycle_authority(&home)
        .get::<std::collections::HashMap<String, JsonValue>>()
        .unwrap()
        .clone();
    let mut state = root["state"]
        .get::<std::collections::HashMap<String, JsonValue>>()
        .unwrap()
        .clone();
    let mut bindings = state["session_bindings"]
        .get::<std::collections::HashMap<String, JsonValue>>()
        .unwrap()
        .clone();
    bindings.insert(
        malformed.to_string(),
        JsonValue::from("malformed".to_string()),
    );
    state.insert("session_bindings".into(), JsonValue::from(bindings));
    root.insert("state".into(), JsonValue::from(state));
    write_lifecycle_authority(&home, JsonValue::from(root));
    assert_eq!(
        store
            .gc_unmanaged(SystemTime::now(), GcControl::RunToCompletion)
            .unwrap()
            .removed_candidates,
        0
    );
    assert!(
        home.join("mailbox")
            .join(format!("{malformed}.jsonl"))
            .exists()
    );
    fs::remove_dir_all(home).ok();
}

#[test]
fn managed_gc_claimant_before_cas_preserves_every_candidate_byte() {
    let home = fresh_home("gc-claim-wins");
    let store = LifecycleStore::new(home.clone());
    let id = "f3333333-3333-4333-8333-333333333333";
    seed_old_unmanaged(&store, &home, id);
    let stopped = store
        .gc_unmanaged(
            SystemTime::now(),
            GcControl::StopAfter(GcCheckpoint::Enumerated),
        )
        .unwrap();
    assert_eq!(stopped.stopped_at, Some(GcCheckpoint::Enumerated));

    let worker = "f1111111-1111-4111-8111-111111111111";
    let generation = "f2222222-2222-4222-8222-222222222222";
    store
        .create_pending(
            pending(
                worker,
                generation,
                Some(id),
                "claude",
                home.to_str().unwrap(),
                relay::store::now_ms() + 30_000,
            ),
            "claim-wins",
        )
        .unwrap();
    let before = fs::read(home.join("registry.json")).unwrap();
    let mailbox = fs::read(home.join("mailbox").join(format!("{id}.jsonl"))).unwrap();
    assert_eq!(
        store
            .gc_unmanaged(SystemTime::now(), GcControl::RunToCompletion)
            .unwrap()
            .removed_candidates,
        0
    );
    assert_eq!(fs::read(home.join("registry.json")).unwrap(), before);
    assert_eq!(
        fs::read(home.join("mailbox").join(format!("{id}.jsonl"))).unwrap(),
        mailbox
    );
    fs::remove_dir_all(home).ok();
}

#[test]
fn managed_gc_gcdel_wins_claimant_writes_nothing_and_exact_epoch_resumes() {
    let home = fresh_home("gc-wins");
    let store = LifecycleStore::new(home.clone());
    let id = "a4333333-3333-4333-8333-333333333333";
    seed_old_unmanaged(&store, &home, id);
    let stopped = store
        .gc_unmanaged(
            SystemTime::now(),
            GcControl::StopAfter(GcCheckpoint::GcDeletingPublished),
        )
        .unwrap();
    assert_eq!(stopped.stopped_at, Some(GcCheckpoint::GcDeletingPublished));
    assert!(matches!(
        store.read_binding(id).unwrap().unwrap().state,
        BindingState::GcDeleting { .. }
    ));
    let before = fs::read(home.join("registry.json")).unwrap();
    assert!(store.hold_unmanaged_epoch(id).is_err());
    assert!(
        store
            .create_pending(
                pending(
                    "a4111111-1111-4111-8111-111111111111",
                    "a4222222-2222-4222-8222-222222222222",
                    Some(id),
                    "claude",
                    home.to_str().unwrap(),
                    relay::store::now_ms() + 30_000,
                ),
                "gc-wins",
            )
            .is_err()
    );
    assert_eq!(fs::read(home.join("registry.json")).unwrap(), before);

    assert_eq!(
        store
            .gc_unmanaged(SystemTime::now(), GcControl::RunToCompletion)
            .unwrap()
            .removed_candidates,
        1
    );
    assert!(!registry_has_binding(&home, id));
    fs::remove_dir_all(home).ok();
}

#[test]
fn managed_gc_crash_after_first_delete_resumes_without_early_record_removal() {
    let home = fresh_home("gc-first-delete");
    let store = LifecycleStore::new(home.clone());
    let id = "b4333333-3333-4333-8333-333333333333";
    seed_old_unmanaged(&store, &home, id);
    let stopped = store
        .gc_unmanaged(
            SystemTime::now(),
            GcControl::StopAfter(GcCheckpoint::FirstSurfaceDeleted),
        )
        .unwrap();
    assert_eq!(stopped.stopped_at, Some(GcCheckpoint::FirstSurfaceDeleted));
    assert!(registry_has_binding(&home, id));
    assert!(
        json_object(&registry(&home))["agents"]
            .get::<std::collections::HashMap<String, JsonValue>>()
            .unwrap()
            .contains_key(id)
    );
    assert_eq!(
        store
            .gc_unmanaged(SystemTime::now(), GcControl::RunToCompletion)
            .unwrap()
            .removed_candidates,
        1
    );
    fs::remove_dir_all(home).ok();
}

#[test]
fn managed_gc_crash_after_lock_unlink_keeps_record_until_final_cas() {
    let home = fresh_home("gc-lock-unlink");
    let store = LifecycleStore::new(home.clone());
    let id = "c4333333-3333-4333-8333-333333333333";
    seed_old_unmanaged(&store, &home, id);
    let stopped = store
        .gc_unmanaged(
            SystemTime::now(),
            GcControl::StopAfter(GcCheckpoint::BindingLockUnlinked),
        )
        .unwrap();
    assert_eq!(stopped.stopped_at, Some(GcCheckpoint::BindingLockUnlinked));
    assert!(registry_has_binding(&home, id));
    assert!(
        !home
            .join("locks")
            .join(format!("binding-{id}.lock"))
            .exists()
    );
    assert_eq!(
        store
            .gc_unmanaged(SystemTime::now(), GcControl::RunToCompletion)
            .unwrap()
            .removed_candidates,
        1
    );
    assert!(!registry_has_binding(&home, id));
    fs::remove_dir_all(home).ok();
}

fn run_managed_hook(
    home: &Path,
    tool: &str,
    worker: &str,
    generation: &str,
    token: &str,
    session: &str,
    cwd: &str,
) -> std::process::Output {
    let input = format!(r#"{{"session_id":"{session}","cwd":"{cwd}","source":"startup"}}"#);
    let mut child = Command::new(env!("CARGO_BIN_EXE_relay"))
        .args(if tool == "codex" {
            vec!["hook", "codex"]
        } else {
            vec!["hook"]
        })
        .env("AGENT_RELAY_HOME", home)
        .env("RELAY_MANAGED_WORKER_ID", worker)
        .env("RELAY_MANAGED_GENERATION", generation)
        .env("RELAY_MANAGED_ATTACH_TOKEN", token)
        .env("RELAY_NO_WATCH", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write as _;
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn managed_attach_hook_claims_before_register_and_drains_only_after_active() {
    let home = fresh_home("hook-active");
    let store = LifecycleStore::new(home.clone());
    let worker = "d1111111-1111-4111-8111-111111111111";
    let generation = "d2222222-2222-4222-8222-222222222222";
    let session = "d3333333-3333-4333-8333-333333333333";
    let cwd = home.join("project");
    fs::create_dir_all(&cwd).unwrap();
    store
        .create_pending(
            pending(
                worker,
                generation,
                Some(session),
                "claude",
                cwd.to_str().unwrap(),
                relay::store::now_ms() + 30_000,
            ),
            "hook-secret",
        )
        .unwrap();
    fs::write(
        home.join("mailbox").join(format!("{session}.jsonl")),
        "{\"from\":\"sender\",\"body\":\"after-active\",\"ts\":\"now\"}\n",
    )
    .unwrap();

    let output = run_managed_hook(
        &home,
        "claude",
        worker,
        generation,
        "hook-secret",
        session,
        cwd.to_str().unwrap(),
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("after-active"));
    assert_eq!(
        store.read_worker(worker).unwrap().unwrap().state,
        ManagedState::Active
    );
    assert!(
        !home
            .join("mailbox")
            .join(format!("{session}.jsonl"))
            .exists()
    );
    fs::remove_dir_all(home).ok();
}

#[test]
fn managed_attach_hook_refusal_stops_before_register_marker_or_mailbox_drain() {
    let home = fresh_home("hook-refuse");
    let store = LifecycleStore::new(home.clone());
    let worker = "e1111111-1111-4111-8111-111111111111";
    let generation = "e2222222-2222-4222-8222-222222222222";
    let session = "e3333333-3333-4333-8333-333333333333";
    let cwd = home.join("project");
    fs::create_dir_all(&cwd).unwrap();
    store
        .create_pending(
            pending(
                worker,
                generation,
                Some(session),
                "claude",
                cwd.to_str().unwrap(),
                relay::store::now_ms() - 1,
            ),
            "expired-hook-secret",
        )
        .unwrap();
    let mailbox = home.join("mailbox").join(format!("{session}.jsonl"));
    fs::write(
        &mailbox,
        "{\"from\":\"sender\",\"body\":\"must-stay\",\"ts\":\"now\"}\n",
    )
    .unwrap();

    let output = run_managed_hook(
        &home,
        "claude",
        worker,
        generation,
        "expired-hook-secret",
        session,
        cwd.to_str().unwrap(),
    );
    assert!(output.status.success());
    let stdout: JsonValue = String::from_utf8(output.stdout).unwrap().parse().unwrap();
    let root = json_object(&stdout);
    assert_eq!(root["continue"].get::<bool>().copied(), Some(false));
    assert!(
        root["stopReason"]
            .get::<String>()
            .unwrap()
            .contains("expired")
    );
    assert!(fs::read_to_string(&mailbox).unwrap().contains("must-stay"));
    assert!(
        !home
            .join("markers")
            .join(relay::store::encode_dir(cwd.to_str().unwrap()))
            .exists()
    );
    fs::remove_dir_all(home).ok();
}

#[test]
fn managed_spawn_omp_waits_for_new_session_and_exact_hook_claim() {
    const TEST: &str = "managed_spawn_omp_waits_for_new_session_and_exact_hook_claim";
    const SESSION: &str = "f3333333-3333-4333-8333-333333333333";
    const OLD: &str = "f4444444-4444-4444-8444-444444444444";
    const DECOY: &str = "f5555555-5555-4555-8555-555555555555";

    fn wait_for(path: &Path) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !path.exists() {
            assert!(Instant::now() < deadline, "timed out waiting for {path:?}");
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn session_file(root: &Path, id: &str, cwd: &Path) {
        let header = JsonValue::from(std::collections::HashMap::from([
            ("type".to_string(), JsonValue::from("session".to_string())),
            ("id".to_string(), JsonValue::from(id.to_string())),
            (
                "cwd".to_string(),
                JsonValue::from(cwd.to_str().unwrap().to_string()),
            ),
        ]));
        fs::write(
            root.join(format!("{id}.jsonl")),
            header.stringify().unwrap() + "\n",
        )
        .unwrap();
    }

    struct Reap(std::process::Child);
    impl Drop for Reap {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    if let Some(home) = std::env::var_os("RELAY_TEST_OMP_CHILD") {
        let home = std::path::PathBuf::from(home);
        let cwd = home.join("project");
        let bucket = home.join("omp-sessions/bucket");
        let worker = std::env::var("RELAY_MANAGED_WORKER_ID").unwrap();
        let generation = std::env::var("RELAY_MANAGED_GENERATION").unwrap();
        let token = std::env::var("RELAY_MANAGED_ATTACH_TOKEN").unwrap();
        assert!(!token.is_empty());
        fs::write(
            home.join("identity"),
            format!("{worker}\n{generation}\n{token}"),
        )
        .unwrap();
        session_file(&bucket, DECOY, &home.join("other-project"));
        session_file(&bucket, SESSION, &cwd);
        fs::write(home.join("files-ready"), "").unwrap();
        wait_for(&home.join("allow-hook"));
        let mut hook = Reap(
            Command::new(env!("CARGO_BIN_EXE_relay"))
                .args(["hook", "omp", "--session", SESSION, "--cwd"])
                .arg(&cwd)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap(),
        );
        let open_stdin = hook.0.stdin.take().unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(status) = hook.0.try_wait().unwrap() {
                assert!(status.success(), "omp hook failed: {status}");
                break;
            }
            assert!(Instant::now() < deadline, "omp hook waited for stdin EOF");
            thread::sleep(Duration::from_millis(10));
        }
        drop(open_stdin);
        fs::write(home.join("hook-done"), "").unwrap();
        wait_for(&home.join("finish"));
        fs::write(home.join("child-done"), "").unwrap();
        return;
    }

    let home = fresh_home("omp-managed-spawn");
    let cwd = home.join("project");
    let bucket = home.join("omp-sessions/bucket");
    fs::create_dir_all(&cwd).unwrap();
    fs::create_dir_all(&bucket).unwrap();
    session_file(&bucket, OLD, &cwd);
    let fake = home.join("fake-omp");
    support::write_executable(
        &fake,
        &format!("#!/bin/sh\nexec \"$RELAY_TEST_BINARY\" --exact {TEST} --nocapture\n"),
    );
    let mut spawn = Reap(
        Command::new(env!("CARGO_BIN_EXE_relay"))
            .arg("spawn")
            .arg(&cwd)
            .args([
                "--tool",
                "omp",
                "--reply-to",
                "observer",
                "--timeout",
                "15",
                "--",
                "inspect this project",
            ])
            .env("AGENT_RELAY_HOME", &home)
            .env("RELAY_OMP_SESSIONS", home.join("omp-sessions"))
            .env("RELAY_SPAWN_CMD_OMP", &fake)
            .env("RELAY_TEST_OMP_CHILD", &home)
            .env("RELAY_TEST_BINARY", std::env::current_exe().unwrap())
            .env("RELAY_NO_WATCH", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    wait_for(&home.join("files-ready"));
    let identity = fs::read_to_string(home.join("identity")).unwrap();
    let mut identity = identity.lines();
    let worker = identity.next().unwrap();
    let generation = identity.next().unwrap();
    let token = identity.next().unwrap();
    let store = LifecycleStore::new(home.clone());
    let pending = store.read_pending_by_token(token).unwrap().unwrap();
    assert_eq!(pending.worker_id, worker);
    assert_eq!(pending.generation, generation);
    assert_eq!(pending.expected_runtime_session_id, None);
    assert_eq!(pending.expected_tool, "omp");
    assert_eq!(pending.expected_cwd, cwd.to_str().unwrap());
    assert_eq!(
        store
            .read_worker(worker)
            .unwrap()
            .unwrap()
            .runtime_session_id,
        None
    );
    assert!(store.read_binding(SESSION).unwrap().is_none());
    let observation_deadline = Instant::now() + Duration::from_millis(400);
    while Instant::now() < observation_deadline {
        assert!(
            spawn.0.try_wait().unwrap().is_none(),
            "session file alone completed spawn before the managed hook claim"
        );
        thread::sleep(Duration::from_millis(10));
    }
    fs::write(home.join("allow-hook"), "").unwrap();
    wait_for(&home.join("hook-done"));
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = spawn.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "spawn did not return after omp hook"
        );
        thread::sleep(Duration::from_millis(10));
    };
    use std::io::Read as _;
    let mut stdout = String::new();
    let mut stderr = String::new();
    spawn
        .0
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    spawn
        .0
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(status.success(), "{stdout}\n{stderr}");
    assert!(
        stdout.contains(SESSION),
        "spawn returned no matching session: {stdout}"
    );
    let active = store.read_worker(worker).unwrap().unwrap();
    assert_eq!(active.state, ManagedState::Active);
    assert_eq!(active.worker_id, worker);
    assert_eq!(active.generation, generation);
    assert_eq!(active.runtime_session_id.as_deref(), Some(SESSION));
    assert_eq!(active.tool, "omp");
    assert_eq!(active.cwd, cwd.to_str().unwrap());
    let binding = store.read_binding(SESSION).unwrap().unwrap();
    assert_eq!(binding.runtime_session_id, SESSION);
    assert_eq!(
        binding.state,
        BindingState::Managed {
            worker_id: worker.to_string(),
            generation: generation.to_string(),
        }
    );
    assert!(store.read_binding(OLD).unwrap().is_none());
    assert!(store.read_binding(DECOY).unwrap().is_none());
    fs::write(home.join("finish"), "").unwrap();
    wait_for(&home.join("child-done"));
    fs::remove_dir_all(home).ok();
}
