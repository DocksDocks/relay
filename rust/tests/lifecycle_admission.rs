use relay::lifecycle::{
    Admission, AttachOptions, BindingState, ChildLaunchSpec, ClaimManagedAttach, ClaimOutcome,
    DoorbellMessage, ExecutionBackend, LifecycleStore, ManagedState, OperationKind,
    PendingAttachSpec, RequiredScope, ValidatedEffort, ValidatedModel,
};
use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tinyjson::JsonValue;

fn fresh_home(tag: &str) -> PathBuf {
    let home = std::env::temp_dir().join(format!(
        "relay-admission-{tag}-{}-{}",
        std::process::id(),
        relay::store::uuid_v4()
    ));
    fs::create_dir_all(&home).unwrap();
    home
}

fn seed_entry(home: &Path, id: &str, tool: &str, cwd: &Path) {
    fs::create_dir_all(cwd).unwrap();
    fs::create_dir_all(home.join("mailbox")).unwrap();
    fs::create_dir_all(home.join("markers")).unwrap();
    fs::create_dir_all(home.join("watchers")).unwrap();
    fs::create_dir_all(home.join("locks")).unwrap();
    let mut entry = HashMap::new();
    entry.insert("id".into(), JsonValue::from(id.to_string()));
    entry.insert(
        "dir".into(),
        JsonValue::from(cwd.to_string_lossy().into_owned()),
    );
    entry.insert("name".into(), JsonValue::from(()));
    entry.insert("tool".into(), JsonValue::from(tool.to_string()));
    entry.insert("lastSeen".into(), JsonValue::from(relay::store::iso_now()));
    entry.insert(
        "server".into(),
        JsonValue::from(cwd.join("appserver.sock").to_string_lossy().into_owned()),
    );
    entry.insert("spawned_via".into(), JsonValue::from(()));
    let mut agents = HashMap::new();
    agents.insert(id.to_string(), JsonValue::from(entry));
    let mut root = HashMap::new();
    root.insert("agents".into(), JsonValue::from(agents));
    root.insert("names".into(), JsonValue::from(HashMap::new()));
    fs::write(
        home.join("registry.json"),
        JsonValue::from(root).format().unwrap(),
    )
    .unwrap();
}

fn pending(
    worker_id: &str,
    generation: &str,
    session: Option<&str>,
    tool: &str,
    cwd: &Path,
) -> PendingAttachSpec {
    PendingAttachSpec {
        worker_id: worker_id.to_string(),
        generation: generation.to_string(),
        expected_runtime_session_id: session.map(str::to_string),
        expected_tool: tool.to_string(),
        expected_cwd: cwd.to_string_lossy().into_owned(),
        expires_at_ms: relay::store::now_ms() + 30_000,
        required_scope: RequiredScope::ProcessOnly,
        execution: ExecutionBackend::SupervisorOwnedProcess,
    }
}

fn claim(
    raw_token: &str,
    worker_id: &str,
    generation: &str,
    session: &str,
    tool: &str,
    cwd: &Path,
) -> ClaimManagedAttach {
    ClaimManagedAttach {
        raw_token: raw_token.to_string(),
        worker_id: worker_id.to_string(),
        generation: generation.to_string(),
        runtime_session_id: session.to_string(),
        tool: tool.to_string(),
        cwd: cwd.to_string_lossy().into_owned(),
    }
}

fn active_worker(
    store: &LifecycleStore,
    cwd: &Path,
    worker: &str,
    generation: &str,
    session: &str,
) {
    store
        .create_pending(
            pending(worker, generation, Some(session), "claude", cwd),
            "active-token",
        )
        .unwrap();
    assert!(matches!(
        store
            .claim_managed_attach(claim(
                "active-token",
                worker,
                generation,
                session,
                "claude",
                cwd,
            ))
            .unwrap(),
        ClaimOutcome::Active { .. }
    ));
}

fn wait_until(mut predicate: impl FnMut() -> bool, message: &str) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !predicate() {
        assert!(Instant::now() < deadline, "{message}");
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn lifecycle_admission_unmanaged_guard_binds_target_kind_and_epoch() {
    let home = fresh_home("unmanaged");
    let cwd = home.join("project");
    let session = "11111111-1111-4111-8111-111111111111";
    seed_entry(&home, session, "claude", &cwd);
    fs::write(
        home.join("mailbox").join(format!("{session}.jsonl")),
        "{\"body\":\"guarded\"}\n",
    )
    .unwrap();
    let store = LifecycleStore::new(home.clone());

    let Admission::Unmanaged(mut guard) = store
        .admit_operation(session, OperationKind::CliInboxDrain)
        .unwrap()
    else {
        panic!("expected unmanaged admission");
    };
    assert_eq!(guard.binding_epoch(), "0");
    assert!(guard.validate_kind(OperationKind::CliInboxDrain).is_ok());
    assert!(guard.validate_kind(OperationKind::McpInboxDrain).is_err());
    let messages = relay::store::drain_with_guard(&mut guard).unwrap();
    assert_eq!(messages.len(), 1);
    assert!(
        !home
            .join("mailbox")
            .join(format!("{session}.jsonl"))
            .exists()
    );
    fs::remove_dir_all(home).ok();
}

#[test]
fn lifecycle_admission_guard_a_cannot_name_or_mutate_b() {
    let home = fresh_home("target");
    let cwd = home.join("project");
    let a = "21111111-1111-4111-8111-111111111111";
    let b = "22222222-2222-4222-8222-222222222222";
    seed_entry(&home, a, "claude", &cwd);
    let mut root: JsonValue = fs::read_to_string(home.join("registry.json"))
        .unwrap()
        .parse()
        .unwrap();
    let root_object = root.get_mut::<HashMap<String, JsonValue>>().unwrap();
    let agents = root_object
        .get_mut("agents")
        .unwrap()
        .get_mut::<HashMap<String, JsonValue>>()
        .unwrap();
    let mut b_entry = agents[a]
        .get::<HashMap<String, JsonValue>>()
        .unwrap()
        .clone();
    b_entry.insert("id".into(), JsonValue::from(b.to_string()));
    agents.insert(b.to_string(), JsonValue::from(b_entry));
    fs::write(home.join("registry.json"), root.format().unwrap()).unwrap();
    fs::write(
        home.join("mailbox").join(format!("{b}.jsonl")),
        "{\"body\":\"b-must-stay\"}\n",
    )
    .unwrap();
    let store = LifecycleStore::new(home.clone());
    let Admission::Unmanaged(mut guard_a) = store
        .admit_operation(a, OperationKind::CliInboxDrain)
        .unwrap()
    else {
        panic!("expected unmanaged admission");
    };
    assert!(
        relay::store::drain_with_guard(&mut guard_a)
            .unwrap()
            .is_empty()
    );
    assert!(
        fs::read_to_string(home.join("mailbox").join(format!("{b}.jsonl")))
            .unwrap()
            .contains("b-must-stay")
    );
    fs::remove_dir_all(home).ok();
}

#[test]
fn lifecycle_admission_stale_unmanaged_guard_refuses_after_claiming() {
    let home = fresh_home("stale");
    let cwd = home.join("project");
    let session = "31111111-1111-4111-8111-111111111111";
    let worker = "32222222-2222-4222-8222-222222222222";
    let generation = "33333333-3333-4333-8333-333333333333";
    seed_entry(&home, session, "claude", &cwd);
    let store = Arc::new(LifecycleStore::with_timeouts(
        home.clone(),
        Duration::from_secs(2),
        Duration::from_secs(1),
    ));
    let Admission::Unmanaged(mut guard) = store
        .admit_operation(session, OperationKind::CliInboxDrain)
        .unwrap()
    else {
        panic!("expected unmanaged admission");
    };
    store
        .create_pending(
            pending(worker, generation, Some(session), "claude", &cwd),
            "stale-token",
        )
        .unwrap();
    let claimant = {
        let store = Arc::clone(&store);
        let cwd = cwd.clone();
        thread::spawn(move || {
            store.claim_managed_attach(claim(
                "stale-token",
                worker,
                generation,
                session,
                "claude",
                &cwd,
            ))
        })
    };
    wait_until(
        || {
            matches!(
                store
                    .read_binding(session)
                    .unwrap()
                    .map(|binding| binding.state),
                Some(BindingState::Claiming { .. })
            )
        },
        "binding never reached Claiming",
    );
    assert!(relay::store::drain_with_guard(&mut guard).is_err());
    drop(guard);
    assert!(matches!(
        claimant.join().unwrap().unwrap(),
        ClaimOutcome::Active { .. }
    ));
    fs::remove_dir_all(home).ok();
}

#[test]
fn lifecycle_admission_publish_fence_precedes_exclusive_drain() {
    let home = fresh_home("fence");
    let cwd = home.join("project");
    let session = "41111111-1111-4111-8111-111111111111";
    let worker = "42222222-2222-4222-8222-222222222222";
    let generation = "43333333-3333-4333-8333-333333333333";
    seed_entry(&home, session, "claude", &cwd);
    let store = LifecycleStore::new(home.clone());
    active_worker(&store, &cwd, worker, generation, session);
    let Admission::Managed(guard) = store
        .admit_operation(session, OperationKind::UserPromptDrain)
        .unwrap()
    else {
        panic!("expected managed admission");
    };

    let intent = store
        .publish_fence(worker, generation, "test fence")
        .unwrap();
    assert_eq!(
        store.read_worker(worker).unwrap().unwrap().state,
        ManagedState::Fencing,
        "Fencing must be durable before reader drain"
    );
    assert!(matches!(
        store
            .admit_operation(session, OperationKind::UserPromptDrain)
            .unwrap(),
        Admission::Refused {
            state: ManagedState::Fencing,
            ..
        }
    ));
    drop(guard);
    let permit = store.drain_prior_operations(intent).unwrap();
    assert_eq!(permit.worker_id(), worker);
    assert_eq!(permit.generation(), generation);
    fs::remove_dir_all(home).ok();
}

#[test]
fn lifecycle_admission_closed_launch_payload_rejects_invalid_and_mismatched_variants() {
    assert!(ValidatedModel::parse("").is_err());
    assert!(ValidatedModel::parse(&"x".repeat(129)).is_err());
    assert!(ValidatedEffort::parse("impossible").is_err());
    assert!(DoorbellMessage::parse(&"x".repeat(16_385)).is_err());
    let options = AttachOptions::new(
        Some(ValidatedModel::parse("opus").unwrap()),
        Some(ValidatedEffort::parse("max").unwrap()),
    );
    let spec = ChildLaunchSpec::AttachResume(options);

    let home = fresh_home("variant");
    let cwd = home.join("project");
    let session = "51111111-1111-4111-8111-111111111111";
    seed_entry(&home, session, "claude", &cwd);
    let store = LifecycleStore::new(home.clone());
    let Admission::Unmanaged(mut guard) = store
        .admit_operation(session, OperationKind::CliInboxDrain)
        .unwrap()
    else {
        panic!("expected unmanaged admission");
    };
    assert!(relay::spawn::run_child_with_guard(&mut guard, spec).is_err());
    fs::remove_dir_all(home).ok();
}

#[test]
fn lifecycle_admission_unmanaged_attach_claiming_kills_waits_and_releases_guard() {
    let home = fresh_home("attach-race");
    let cwd = home.join("project");
    let session = "61111111-1111-4111-8111-111111111111";
    let worker = "62222222-2222-4222-8222-222222222222";
    let generation = "63333333-3333-4333-8333-333333333333";
    seed_entry(&home, session, "claude", &cwd);
    let sentinel = home.join("sentinel.log");
    let stub_dir = home.join("bin");
    fs::create_dir_all(&stub_dir).unwrap();
    let stub = stub_dir.join("claude");
    fs::write(
        &stub,
        "#!/bin/sh\nwhile :; do printf x >> \"$RELAY_TEST_SENTINEL\"; sleep 0.01; done\n",
    )
    .unwrap();
    fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        stub_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let mut attach = Command::new(env!("CARGO_BIN_EXE_relay"))
        .args(["attach", session])
        .env("AGENT_RELAY_HOME", &home)
        .env("PATH", path)
        .env("RELAY_TEST_SENTINEL", &sentinel)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_until(
        || fs::metadata(&sentinel).is_ok_and(|metadata| metadata.len() >= 5),
        "attach child never wrote its sentinel",
    );

    let store = Arc::new(LifecycleStore::with_timeouts(
        home.clone(),
        Duration::from_secs(2),
        Duration::from_secs(1),
    ));
    store
        .create_pending(
            pending(worker, generation, Some(session), "claude", &cwd),
            "race-token",
        )
        .unwrap();
    let claimant = {
        let store = Arc::clone(&store);
        let cwd = cwd.clone();
        thread::spawn(move || {
            store.claim_managed_attach(claim(
                "race-token",
                worker,
                generation,
                session,
                "claude",
                &cwd,
            ))
        })
    };
    wait_until(
        || {
            matches!(
                store
                    .read_binding(session)
                    .unwrap()
                    .map(|binding| binding.state),
                Some(BindingState::Claiming { .. })
            )
        },
        "attach race never reached Claiming",
    );
    let status = attach.wait().unwrap();
    assert!(
        !status.success(),
        "cancelled attach must not report success"
    );
    assert!(matches!(
        claimant.join().unwrap().unwrap(),
        ClaimOutcome::Active { .. }
    ));
    let length_after_wait = fs::metadata(&sentinel).unwrap().len();
    thread::sleep(Duration::from_millis(150));
    assert_eq!(
        fs::metadata(&sentinel).unwrap().len(),
        length_after_wait,
        "sentinel wrote after child wait/reap and Active publication"
    );
    fs::remove_dir_all(home).ok();
}

#[test]
fn lifecycle_admission_reentry_matrix() {
    let kinds = [
        OperationKind::SessionStartDrain,
        OperationKind::UserPromptDrain,
        OperationKind::CliInboxDrain,
        OperationKind::McpInboxDrain,
        OperationKind::ChannelDeliver,
        OperationKind::WatchInject,
        OperationKind::WatchAutoTurn,
        OperationKind::WatchAck,
        OperationKind::WatchWakeFallback,
        OperationKind::WakeAppServer,
        OperationKind::WakeCli,
        OperationKind::AttachResume,
        OperationKind::Deliver,
        OperationKind::InitialTurn,
    ];
    for (index, kind) in kinds.into_iter().enumerate() {
        let home = fresh_home(&format!("matrix-{index}"));
        let cwd = home.join("project");
        let session = format!("7{index:07}-1111-4111-8111-111111111111");
        seed_entry(&home, &session, "claude", &cwd);
        let store = LifecycleStore::new(home.clone());
        let admission = store.admit_operation(&session, kind).unwrap();
        let guard = match admission {
            Admission::Unmanaged(guard) => guard,
            _ => panic!("expected unmanaged admission"),
        };
        assert!(guard.validate_kind(kind).is_ok());
        let wrong = if kind == OperationKind::CliInboxDrain {
            OperationKind::McpInboxDrain
        } else {
            OperationKind::CliInboxDrain
        };
        assert!(guard.validate_kind(wrong).is_err());
        println!("PASS reentry kind={}", kind.as_str());
        fs::remove_dir_all(home).ok();
    }
}
