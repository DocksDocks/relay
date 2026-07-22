use relay::fanout::FanoutStore;
use relay::lifecycle::{
    ClaimManagedAttach, ClaimOutcome, ExecutionBackend, LifecycleStore, PendingAttachSpec,
    RequiredScope,
};
use relay::store;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use tinyjson::JsonValue;

pub fn seed_entry(home: &Path, id: &str, cwd: &Path) {
    for dir in ["mailbox", "markers", "watchers", "locks"] {
        fs::create_dir_all(home.join(dir)).unwrap();
    }
    let mut root = HashMap::new();
    let mut agents = HashMap::new();
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
    root.insert("names".into(), JsonValue::from(HashMap::new()));
    fs::write(
        home.join("registry.json"),
        JsonValue::from(root).format().unwrap(),
    )
    .unwrap();
}

pub fn activate_worker(
    fanout: &FanoutStore,
    record_id: &str,
    runtime_session_id: &str,
    cwd: &Path,
) -> String {
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
    assert!(matches!(
        lifecycle
            .claim_managed_attach(ClaimManagedAttach {
                raw_token: token,
                worker_id: worker.clone(),
                generation,
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
    worker
}

pub fn force_terminal_releasable(home: &Path, worker: &str) {
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
