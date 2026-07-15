pub mod support;

use relay::lifecycle::{
    Admission, ClaimManagedAttach, ClaimOutcome, ExecutionBackend, LifecycleStore, ManagedState,
    OperationKind, PendingAttachSpec, RequiredScope, TerminalAction,
};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::Command;
use support::{fresh_home, write_executable};
use tinyjson::JsonValue;

fn pending(
    worker_id: &str,
    generation: &str,
    expected_runtime_session_id: Option<&str>,
    tool: &str,
    cwd: &Path,
) -> PendingAttachSpec {
    PendingAttachSpec {
        worker_id: worker_id.to_string(),
        generation: generation.to_string(),
        expected_runtime_session_id: expected_runtime_session_id.map(str::to_string),
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
    runtime_session_id: &str,
    cwd: &Path,
) -> ClaimManagedAttach {
    ClaimManagedAttach {
        raw_token: raw_token.to_string(),
        worker_id: worker_id.to_string(),
        generation: generation.to_string(),
        runtime_session_id: runtime_session_id.to_string(),
        tool: "claude".to_string(),
        cwd: cwd.to_string_lossy().into_owned(),
    }
}

fn json_object(value: &JsonValue) -> &HashMap<String, JsonValue> {
    value.get::<HashMap<String, JsonValue>>().unwrap()
}

fn old_registry_round_trip(home: &Path) {
    let registry: JsonValue = fs::read_to_string(home.join("registry.json"))
        .unwrap()
        .parse()
        .unwrap();
    let registry = json_object(&registry);
    let mut legacy = HashMap::new();
    legacy.insert("agents".to_string(), registry["agents"].clone());
    legacy.insert("names".to_string(), registry["names"].clone());
    fs::write(
        home.join("registry.json"),
        JsonValue::from(legacy).format().unwrap(),
    )
    .unwrap();
}

#[test]
fn lifecycle_authority_survives_legacy_registry_round_trip() {
    let home = fresh_home("old-writer");
    let store = LifecycleStore::new(home.clone());
    let worker = "11111111-1111-4111-8111-111111111111";
    let generation = "22222222-2222-4222-8222-222222222222";

    store
        .create_pending(
            pending(worker, generation, None, "codex", &home),
            "old-writer-token",
        )
        .unwrap();

    let registry: JsonValue = fs::read_to_string(home.join("registry.json"))
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        json_object(&registry)
            .keys()
            .cloned()
            .collect::<std::collections::HashSet<_>>(),
        ["agents".to_string(), "names".to_string()]
            .into_iter()
            .collect()
    );
    let authority: JsonValue = fs::read_to_string(home.join("lifecycle-v1.json"))
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        json_object(&authority)["schema_version"],
        JsonValue::from("1".to_string())
    );
    assert!(
        json_object(&json_object(&authority)["state"])["managed_workers"]
            .get::<HashMap<String, JsonValue>>()
            .unwrap()
            .contains_key(worker)
    );

    old_registry_round_trip(&home);
    let recovered = store.read_worker(worker).unwrap().unwrap();
    assert_eq!(recovered.generation, generation);
    assert_eq!(recovered.state, ManagedState::Attaching);

    fs::remove_dir_all(home).ok();
}

#[test]
fn missing_authority_does_not_trust_registry_lifecycle_fields() {
    let home = fresh_home("missing-authority");
    let store = LifecycleStore::new(home.clone());
    let forged_worker = "21111111-1111-4111-8111-111111111111";
    let forged_generation = "22222222-2222-4222-8222-222222222223";
    store
        .create_pending(
            pending(forged_worker, forged_generation, None, "codex", &home),
            "forged-registry-token",
        )
        .unwrap();

    let authority: JsonValue = fs::read_to_string(home.join("lifecycle-v1.json"))
        .unwrap()
        .parse()
        .unwrap();
    let state = json_object(&json_object(&authority)["state"]);
    let registry: JsonValue = fs::read_to_string(home.join("registry.json"))
        .unwrap()
        .parse()
        .unwrap();
    let mut registry = json_object(&registry).clone();
    for (key, value) in state {
        registry.insert(key.clone(), value.clone());
    }
    fs::write(
        home.join("registry.json"),
        JsonValue::from(registry).format().unwrap(),
    )
    .unwrap();
    fs::remove_file(home.join("lifecycle-v1.json")).unwrap();

    assert_eq!(store.read_worker(forged_worker).unwrap(), None);

    let real_worker = "21111111-1111-4111-8111-111111111112";
    let real_generation = "22222222-2222-4222-8222-222222222224";
    store
        .create_pending(
            pending(real_worker, real_generation, None, "codex", &home),
            "real-authority-token",
        )
        .unwrap();
    assert_eq!(store.read_worker(forged_worker).unwrap(), None);
    assert_eq!(
        store.read_worker(real_worker).unwrap().unwrap().generation,
        real_generation
    );
    let registry: JsonValue = fs::read_to_string(home.join("registry.json"))
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        json_object(&registry)
            .keys()
            .cloned()
            .collect::<std::collections::HashSet<_>>(),
        ["agents".to_string(), "names".to_string()]
            .into_iter()
            .collect()
    );

    fs::remove_dir_all(home).ok();
}

#[test]
fn malformed_lifecycle_authority_fails_closed() {
    let home = fresh_home("malformed");
    let store = LifecycleStore::new(home.clone());
    let worker = "31111111-1111-4111-8111-111111111111";
    let generation = "32222222-2222-4222-8222-222222222222";
    store
        .create_pending(
            pending(worker, generation, None, "codex", &home),
            "malformed-token",
        )
        .unwrap();

    fs::write(home.join("lifecycle-v1.json"), "{\"schema_version\":\"1\"}").unwrap();
    let error = store.read_worker(worker).unwrap_err();
    assert!(error.contains("malformed lifecycle authority"), "{error}");
    assert_eq!(
        fs::read_to_string(home.join("lifecycle-v1.json")).unwrap(),
        "{\"schema_version\":\"1\"}"
    );

    fs::remove_dir_all(home).ok();
}

#[test]
fn terminal_release_requires_owned_reap_and_both_terminal_modes_are_idempotent() {
    let retained_home = fresh_home("retained");
    let retained_store = LifecycleStore::new(retained_home.clone());
    let retained_worker = "41111111-1111-4111-8111-111111111111";
    let retained_generation = "42222222-2222-4222-8222-222222222222";
    retained_store
        .create_pending(
            pending(
                retained_worker,
                retained_generation,
                None,
                "codex",
                &retained_home,
            ),
            "retained-token",
        )
        .unwrap();
    let fencing = retained_store
        .transition_worker(
            retained_worker,
            retained_generation,
            "1",
            ManagedState::Fencing,
        )
        .unwrap();
    let fenced = retained_store
        .transition_worker(
            retained_worker,
            retained_generation,
            &fencing.version,
            ManagedState::Fenced,
        )
        .unwrap();
    assert!(
        retained_store
            .terminalize_worker(
                retained_worker,
                retained_generation,
                &fenced.version,
                TerminalAction::Release,
                "no reap proof",
            )
            .is_err()
    );
    let retained = retained_store
        .terminalize_worker(
            retained_worker,
            retained_generation,
            &fenced.version,
            TerminalAction::Abandon,
            "operator abandoned unproved worker",
        )
        .unwrap();
    assert_eq!(retained.state, ManagedState::TerminalRetained);
    let repeated = retained_store
        .terminalize_worker(
            retained_worker,
            retained_generation,
            &fenced.version,
            TerminalAction::Abandon,
            "operator abandoned unproved worker",
        )
        .unwrap();
    assert_eq!(repeated.version, retained.version);
    assert!(
        retained_store
            .terminalize_worker(
                retained_worker,
                retained_generation,
                &retained.version,
                TerminalAction::Release,
                "conflicting release",
            )
            .is_err()
    );
    assert!(
        retained_store
            .transition_worker(
                retained_worker,
                retained_generation,
                &retained.version,
                ManagedState::Fencing,
            )
            .is_err()
    );

    let released_home = fresh_home("released");
    let released_store = LifecycleStore::new(released_home.clone());
    let released_worker = "51111111-1111-4111-8111-111111111111";
    let released_generation = "52222222-2222-4222-8222-222222222222";
    let session = "53333333-3333-4333-8333-333333333333";
    let cwd = released_home.join("project");
    fs::create_dir_all(&cwd).unwrap();
    released_store
        .create_pending(
            pending(
                released_worker,
                released_generation,
                Some(session),
                "claude",
                &cwd,
            ),
            "released-token",
        )
        .unwrap();
    assert!(matches!(
        released_store
            .claim_managed_attach(claim(
                "released-token",
                released_worker,
                released_generation,
                session,
                &cwd,
            ))
            .unwrap(),
        ClaimOutcome::Active { .. }
    ));
    let bin = released_home.join("bin");
    fs::create_dir_all(&bin).unwrap();
    write_executable(&bin.join("claude"), "#!/bin/sh\nexit 0\n");
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let output = Command::new(env!("CARGO_BIN_EXE_relay"))
        .args(["attach", session])
        .env("AGENT_RELAY_HOME", &released_home)
        .env("PATH", path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let active = released_store
        .read_worker(released_worker)
        .unwrap()
        .unwrap();
    let fencing = released_store
        .transition_worker(
            released_worker,
            released_generation,
            &active.version,
            ManagedState::Fencing,
        )
        .unwrap();
    let fenced = released_store
        .transition_worker(
            released_worker,
            released_generation,
            &fencing.version,
            ManagedState::Fenced,
        )
        .unwrap();
    let released = released_store
        .terminalize_worker(
            released_worker,
            released_generation,
            &fenced.version,
            TerminalAction::Release,
            "supervisor-owned child reaped",
        )
        .unwrap();
    assert_eq!(released.state, ManagedState::TerminalReleasable);
    let receipt = released.release_receipt.as_ref().unwrap();
    assert_eq!(receipt.mode, "release");
    assert_eq!(receipt.evidence_sha256.len(), 64);
    assert_ne!(receipt.evidence_sha256, "0".repeat(64));
    let repeated = released_store
        .terminalize_worker(
            released_worker,
            released_generation,
            &fenced.version,
            TerminalAction::Release,
            "supervisor-owned child reaped",
        )
        .unwrap();
    assert_eq!(repeated.version, released.version);
    assert!(matches!(
        released_store
            .admit_operation(released_worker, OperationKind::AttachResume)
            .unwrap(),
        Admission::Refused {
            state: ManagedState::TerminalReleasable,
            ..
        }
    ));

    fs::remove_dir_all(retained_home).ok();
    fs::remove_dir_all(released_home).ok();
}

#[test]
fn terminal_release_rejects_any_matching_tombstone_without_exact_reap_proof() {
    let home = fresh_home("mixed-reap-proof");
    let store = LifecycleStore::new(home.clone());
    let worker = "61111111-1111-4111-8111-111111111111";
    let generation = "62222222-2222-4222-8222-222222222222";
    let session = "63333333-3333-4333-8333-333333333333";
    let cwd = home.join("project");
    fs::create_dir_all(&cwd).unwrap();
    store
        .create_pending(
            pending(worker, generation, Some(session), "claude", &cwd),
            "mixed-proof-token",
        )
        .unwrap();
    assert!(matches!(
        store
            .claim_managed_attach(claim(
                "mixed-proof-token",
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
    write_executable(&bin.join("claude"), "#!/bin/sh\nexit 0\n");
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let output = Command::new(env!("CARGO_BIN_EXE_relay"))
        .args(["attach", session])
        .env("AGENT_RELAY_HOME", &home)
        .env("PATH", path)
        .output()
        .unwrap();
    assert!(output.status.success());

    let authority_path = home.join("lifecycle-v1.json");
    let authority: JsonValue = fs::read_to_string(&authority_path)
        .unwrap()
        .parse()
        .unwrap();
    let mut authority = json_object(&authority).clone();
    let mut state = json_object(&authority["state"]).clone();
    let mut tombstones = json_object(&state["operation_tombstones"]).clone();
    let mut unproved = json_object(tombstones.values().next().unwrap()).clone();
    unproved.insert(
        "operation_id".to_string(),
        JsonValue::from("64444444-4444-4444-8444-444444444444".to_string()),
    );
    unproved.insert("reap_proof".to_string(), JsonValue::from(()));
    tombstones.insert(
        "64444444-4444-4444-8444-444444444444".to_string(),
        JsonValue::from(unproved),
    );
    state.insert(
        "operation_tombstones".to_string(),
        JsonValue::from(tombstones),
    );
    authority.insert("state".to_string(), JsonValue::from(state));
    fs::write(authority_path, JsonValue::from(authority).format().unwrap()).unwrap();

    let active = store.read_worker(worker).unwrap().unwrap();
    let fencing = store
        .transition_worker(worker, generation, &active.version, ManagedState::Fencing)
        .unwrap();
    let fenced = store
        .transition_worker(worker, generation, &fencing.version, ManagedState::Fenced)
        .unwrap();
    let error = store
        .terminalize_worker(
            worker,
            generation,
            &fenced.version,
            TerminalAction::Release,
            "mixed proof set must refuse",
        )
        .unwrap_err();
    assert!(error.contains("exact supervisor-owned process reap proof"));
    assert_eq!(
        store.read_worker(worker).unwrap().unwrap().state,
        ManagedState::Fenced
    );

    fs::remove_dir_all(home).ok();
}
