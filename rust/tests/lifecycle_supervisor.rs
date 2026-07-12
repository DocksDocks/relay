use relay::lifecycle::{
    BindingState, ClaimManagedAttach, ClaimOutcome, ExecutionBackend, ExternalCustody,
    LifecycleStore, ManagedState, OperationKind, PendingAttachSpec, RequiredScope, SupervisorState,
};
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tinyjson::JsonValue;

fn fresh_home(tag: &str) -> PathBuf {
    let home = std::env::temp_dir().join(format!(
        "relay-supervisor-{tag}-{}-{}",
        std::process::id(),
        relay::store::uuid_v4()
    ));
    fs::create_dir_all(&home).unwrap();
    home
}

fn seed_entry(home: &Path, id: &str, tool: &str, cwd: &Path) {
    fs::create_dir_all(cwd).unwrap();
    for dir in ["mailbox", "markers", "watchers", "locks"] {
        fs::create_dir_all(home.join(dir)).unwrap();
    }
    let mut entry = HashMap::new();
    entry.insert("id".into(), JsonValue::from(id.to_string()));
    entry.insert(
        "dir".into(),
        JsonValue::from(cwd.to_string_lossy().into_owned()),
    );
    entry.insert("name".into(), JsonValue::from(()));
    entry.insert("tool".into(), JsonValue::from(tool.to_string()));
    entry.insert("lastSeen".into(), JsonValue::from(relay::store::iso_now()));
    entry.insert("server".into(), JsonValue::from(()));
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

fn write_executable(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn wait_until(timeout: Duration, message: &str, mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !predicate() {
        assert!(Instant::now() < deadline, "{message}");
        thread::sleep(Duration::from_millis(20));
    }
}

fn pending(worker: &str, generation: &str, session: &str, cwd: &Path) -> PendingAttachSpec {
    PendingAttachSpec {
        worker_id: worker.to_string(),
        generation: generation.to_string(),
        expected_runtime_session_id: Some(session.to_string()),
        expected_tool: "claude".to_string(),
        expected_cwd: cwd.to_string_lossy().into_owned(),
        expires_at_ms: relay::store::now_ms() + 30_000,
        required_scope: RequiredScope::ProcessOnly,
        execution: ExecutionBackend::SupervisorOwnedProcess,
    }
}

fn claim(
    token: &str,
    worker: &str,
    generation: &str,
    session: &str,
    cwd: &Path,
) -> ClaimManagedAttach {
    ClaimManagedAttach {
        raw_token: token.to_string(),
        worker_id: worker.to_string(),
        generation: generation.to_string(),
        runtime_session_id: session.to_string(),
        tool: "claude".to_string(),
        cwd: cwd.to_string_lossy().into_owned(),
    }
}

#[test]
fn lifecycle_supervisor_pipe_preserves_bytes_eof_streams_and_reap_custody() {
    let home = fresh_home("pipe");
    let cwd = home.join("project");
    let session = "11111111-1111-4111-8111-111111111111";
    seed_entry(&home, session, "claude", &cwd);
    let bin = home.join("bin");
    fs::create_dir_all(&bin).unwrap();
    write_executable(
        &bin.join("claude"),
        "#!/bin/sh\ninput=$(cat)\nprintf 'stdout:%s' \"$input\"\nprintf 'stderr:%s' \"$input\" >&2\nexit 7\n",
    );
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let mut child = Command::new(env!("CARGO_BIN_EXE_relay"))
        .args(["attach", session])
        .env("AGENT_RELAY_HOME", &home)
        .env("PATH", path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"exact-input")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(7));
    assert_eq!(output.stdout, b"stdout:exact-input");
    assert!(String::from_utf8_lossy(&output.stderr).contains("stderr:exact-input"));

    let store = LifecycleStore::new(home.clone());
    let operations = store.read_operations_for_session(session).unwrap();
    assert_eq!(operations.len(), 1);
    assert!(matches!(
        operations[0].custody,
        ExternalCustody::ChildReaped { exit_status: 7, .. }
    ));
    assert!(operations[0].terminal);
    fs::remove_dir_all(home).ok();
}

#[test]
fn lifecycle_supervisor_wake_uses_closed_stdin_and_separate_output() {
    let home = fresh_home("closed");
    let cwd = home.join("project");
    let session = "21111111-1111-4111-8111-111111111111";
    seed_entry(&home, session, "claude", &cwd);
    let stub = home.join("wake-stub");
    write_executable(
        &stub,
        "#!/bin/sh\nif IFS= read -r line; then printf 'unexpected:%s' \"$line\"; exit 9; fi\nprintf closed-stdin\nprintf separate-stderr >&2\n",
    );
    let output = Command::new(env!("CARGO_BIN_EXE_relay"))
        .args(["wake", session, "doorbell"])
        .env("AGENT_RELAY_HOME", &home)
        .env("RELAY_WAKE_CMD_CLAUDE", &stub)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"closed-stdin");
    assert!(String::from_utf8_lossy(&output.stderr).contains("separate-stderr"));
    let operations = LifecycleStore::new(home.clone())
        .read_operations_for_session(session)
        .unwrap();
    assert!(matches!(
        operations.as_slice(),
        [operation]
            if operation.kind == OperationKind::WakeCli
                && matches!(operation.custody, ExternalCustody::ChildReaped { .. })
    ));
    fs::remove_dir_all(home).ok();
}

#[test]
fn lifecycle_supervisor_disconnect_linearizes_unmanaged_cancel_and_reap() {
    let home = fresh_home("disconnect");
    let cwd = home.join("project");
    let session = "31111111-1111-4111-8111-111111111111";
    seed_entry(&home, session, "claude", &cwd);
    let bin = home.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let sentinel = home.join("alive");
    write_executable(
        &bin.join("claude"),
        "#!/bin/sh\ntrap '' TERM HUP INT\nwhile :; do printf x >> \"$RELAY_TEST_SENTINEL\"; sleep 0.02; done\n",
    );
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let mut attach = Command::new(env!("CARGO_BIN_EXE_relay"))
        .args(["attach", session])
        .env("AGENT_RELAY_HOME", &home)
        .env("RELAY_TEST_SENTINEL", &sentinel)
        .env("PATH", path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_until(
        Duration::from_secs(3),
        "supervised child did not start",
        || fs::metadata(&sentinel).is_ok_and(|metadata| metadata.len() >= 3),
    );
    attach.kill().unwrap();
    attach.wait().unwrap();

    let store = LifecycleStore::new(home.clone());
    wait_until(
        Duration::from_secs(5),
        "unmanaged child was not reaped",
        || {
            store
                .read_operations_for_session(session)
                .is_ok_and(|operations| {
                    operations.iter().any(|operation| {
                        operation.cancelled
                            && operation.terminal
                            && matches!(operation.custody, ExternalCustody::ChildReaped { .. })
                    })
                })
        },
    );
    let binding = store.read_binding(session).unwrap().unwrap();
    assert_eq!(binding.binding_epoch, "1");
    assert_eq!(binding.state, BindingState::Unmanaged);
    assert!(
        store
            .lifecycle_audit_contains("unmanaged-cancel-published", session)
            .unwrap()
    );
    let stable = fs::metadata(&sentinel).unwrap().len();
    thread::sleep(Duration::from_millis(150));
    assert_eq!(fs::metadata(&sentinel).unwrap().len(), stable);
    fs::remove_dir_all(home).ok();
}

#[test]
fn lifecycle_supervisor_watchdog_marks_managed_worker_on_supervisor_death() {
    let home = fresh_home("watchdog");
    let cwd = home.join("project");
    let session = "41111111-1111-4111-8111-111111111111";
    let worker = "42222222-2222-4222-8222-222222222222";
    let generation = "43333333-3333-4333-8333-333333333333";
    seed_entry(&home, session, "claude", &cwd);
    let store = LifecycleStore::new(home.clone());
    store
        .create_pending(pending(worker, generation, session, &cwd), "watchdog-token")
        .unwrap();
    assert!(matches!(
        store
            .claim_managed_attach(claim("watchdog-token", worker, generation, session, &cwd,))
            .unwrap(),
        ClaimOutcome::Active { .. }
    ));
    let bin = home.join("bin");
    fs::create_dir_all(&bin).unwrap();
    write_executable(
        &bin.join("claude"),
        "#!/bin/sh\ntrap '' TERM HUP INT\nwhile :; do sleep 1; done\n",
    );
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let mut attach = Command::new(env!("CARGO_BIN_EXE_relay"))
        .args(["attach", session])
        .env("AGENT_RELAY_HOME", &home)
        .env("PATH", path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut supervisor_pid = None;
    wait_until(
        Duration::from_secs(3),
        "supervisor never became Ready",
        || {
            supervisor_pid = store
                .read_supervisor_for_session(session)
                .ok()
                .flatten()
                .filter(|record| record.state == SupervisorState::Ready)
                .map(|record| record.process.pid);
            supervisor_pid.is_some()
        },
    );
    let status = Command::new("kill")
        .args(["-KILL", &supervisor_pid.unwrap().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
    wait_until(
        Duration::from_secs(5),
        "watchdog did not publish authority loss",
        || {
            store.read_worker(worker).is_ok_and(|value| {
                value.is_some_and(|record| record.state == ManagedState::FencingUnconfirmed)
            })
        },
    );
    let operation = store
        .read_operations_for_session(session)
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert!(matches!(
        operation.custody,
        ExternalCustody::LostAuthority { .. }
    ));
    attach.kill().ok();
    attach.wait().ok();
    fs::remove_dir_all(home).ok();
}

#[test]
fn managed_attach_cancel_first_reap_transfers_waiting_claim_exactly_once() {
    let home = fresh_home("cancel-first-claim");
    let cwd = home.join("project");
    let session = "51111111-1111-4111-8111-111111111111";
    let worker = "52222222-2222-4222-8222-222222222222";
    let generation = "53333333-3333-4333-8333-333333333333";
    seed_entry(&home, session, "claude", &cwd);
    let store = LifecycleStore::new(home.clone());
    store
        .create_pending(
            pending(worker, generation, session, &cwd),
            "cancel-first-token",
        )
        .unwrap();
    let bin = home.join("bin");
    fs::create_dir_all(&bin).unwrap();
    write_executable(
        &bin.join("claude"),
        "#!/bin/sh\ntrap '' TERM HUP INT\nwhile :; do sleep 1; done\n",
    );
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let mut attach = Command::new(env!("CARGO_BIN_EXE_relay"))
        .args(["attach", session])
        .env("AGENT_RELAY_HOME", &home)
        .env("RELAY_TEST_SUPERVISOR_CANCEL_BARRIER_MS", "500")
        .env("PATH", path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_until(
        Duration::from_secs(3),
        "owned operation never started",
        || {
            store
                .read_operations_for_session(session)
                .is_ok_and(|operations| {
                    operations.iter().any(|operation| {
                        matches!(operation.custody, ExternalCustody::ChildOwned { .. })
                    })
                })
        },
    );
    attach.kill().unwrap();
    attach.wait().unwrap();
    wait_until(
        Duration::from_secs(2),
        "disconnect did not publish cancel-first",
        || {
            store.read_binding(session).is_ok_and(|binding| {
                binding.is_some_and(|binding| {
                    matches!(binding.state, BindingState::UnmanagedCanceling { .. })
                })
            })
        },
    );
    let outcome = store
        .claim_managed_attach(claim(
            "cancel-first-token",
            worker,
            generation,
            session,
            &cwd,
        ))
        .unwrap();
    assert!(matches!(
        outcome,
        ClaimOutcome::Active {
            duplicate: false,
            ..
        }
    ));
    let binding = store.read_binding(session).unwrap().unwrap();
    assert_eq!(binding.binding_epoch, "1");
    assert!(matches!(binding.state, BindingState::Managed { .. }));
    assert_eq!(
        store.read_worker(worker).unwrap().unwrap().state,
        ManagedState::Active
    );
    fs::remove_dir_all(home).ok();
}

#[test]
fn managed_attach_cancel_deadline_binds_fencing_unconfirmed_never_active() {
    let home = fresh_home("cancel-deadline");
    let cwd = home.join("project");
    let session = "61111111-1111-4111-8111-111111111111";
    let worker = "62222222-2222-4222-8222-222222222222";
    let generation = "63333333-3333-4333-8333-333333333333";
    seed_entry(&home, session, "claude", &cwd);
    let store = LifecycleStore::with_timeouts(
        home.clone(),
        Duration::from_millis(100),
        Duration::from_millis(100),
    );
    store
        .create_pending(pending(worker, generation, session, &cwd), "deadline-token")
        .unwrap();
    let bin = home.join("bin");
    fs::create_dir_all(&bin).unwrap();
    write_executable(
        &bin.join("claude"),
        "#!/bin/sh\ntrap '' TERM HUP INT\nwhile :; do sleep 1; done\n",
    );
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let mut attach = Command::new(env!("CARGO_BIN_EXE_relay"))
        .args(["attach", session])
        .env("AGENT_RELAY_HOME", &home)
        .env("RELAY_TEST_SUPERVISOR_CANCEL_BARRIER_MS", "1000")
        .env("PATH", path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_until(
        Duration::from_secs(3),
        "owned operation never started",
        || {
            store
                .read_operations_for_session(session)
                .is_ok_and(|operations| {
                    operations.iter().any(|operation| {
                        matches!(operation.custody, ExternalCustody::ChildOwned { .. })
                    })
                })
        },
    );
    attach.kill().unwrap();
    attach.wait().unwrap();
    wait_until(
        Duration::from_secs(2),
        "disconnect did not publish cancel-first",
        || {
            store.read_binding(session).is_ok_and(|binding| {
                binding.is_some_and(|binding| {
                    matches!(binding.state, BindingState::UnmanagedCanceling { .. })
                })
            })
        },
    );
    let outcome = store
        .claim_managed_attach(claim("deadline-token", worker, generation, session, &cwd))
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
    assert_eq!(
        store.read_worker(worker).unwrap().unwrap().state,
        ManagedState::FencingUnconfirmed
    );
    wait_until(
        Duration::from_secs(3),
        "late child reap did not arrive",
        || {
            store
                .read_operations_for_session(session)
                .is_ok_and(|operations| operations.iter().any(|operation| operation.terminal))
        },
    );
    assert_eq!(
        store.read_worker(worker).unwrap().unwrap().state,
        ManagedState::FencingUnconfirmed,
        "late reap must not resurrect Active"
    );
    fs::remove_dir_all(home).ok();
}

#[test]
fn lifecycle_supervisor_watchdog_first_loss_refuses_next_reentry() {
    let home = fresh_home("watchdog-first");
    let cwd = home.join("project");
    let session = "71111111-1111-4111-8111-111111111111";
    let worker = "72222222-2222-4222-8222-222222222222";
    let generation = "73333333-3333-4333-8333-333333333333";
    seed_entry(&home, session, "claude", &cwd);
    let store = LifecycleStore::new(home.clone());
    store
        .create_pending(
            pending(worker, generation, session, &cwd),
            "watchdog-first-token",
        )
        .unwrap();
    assert!(matches!(
        store
            .claim_managed_attach(claim(
                "watchdog-first-token",
                worker,
                generation,
                session,
                &cwd,
            ))
            .unwrap(),
        ClaimOutcome::Active { .. }
    ));
    let bin = home.join("bin");
    fs::create_dir_all(&bin).unwrap();
    write_executable(
        &bin.join("claude"),
        "#!/bin/sh\ntrap '' TERM HUP INT\nwhile :; do sleep 1; done\n",
    );
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let mut attach = Command::new(env!("CARGO_BIN_EXE_relay"))
        .args(["attach", session])
        .env("AGENT_RELAY_HOME", &home)
        .env("PATH", path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut watchdog_pid = None;
    wait_until(
        Duration::from_secs(3),
        "watchdog record never appeared",
        || {
            watchdog_pid = store
                .read_watchdog_for_session(session)
                .ok()
                .flatten()
                .map(|record| record.process.pid);
            watchdog_pid.is_some()
        },
    );
    assert!(
        Command::new("kill")
            .args(["-KILL", &watchdog_pid.unwrap().to_string()])
            .status()
            .unwrap()
            .success()
    );
    thread::sleep(Duration::from_millis(50));
    assert!(
        store
            .admit_operation(session, OperationKind::UserPromptDrain)
            .is_err(),
        "re-entry must refuse after watchdog generation loss"
    );
    assert_eq!(
        store.read_worker(worker).unwrap().unwrap().state,
        ManagedState::FencingUnconfirmed
    );
    attach.wait().ok();
    fs::remove_dir_all(home).ok();
}
