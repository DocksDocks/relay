pub mod support;

use relay::workspace::authority::atomic_create_jcs;
use relay::workspace::git::OpenedRepository;
use relay::workspace::resources::{allocate_resources, executable_sha256, provider_registry_path};
use relay::workspace::schema::{
    ClosedJcs, JcsValue, ResourceDecisionV1, ResourceProviderRegistrationV1,
    ResourceProviderRegistryV1, jcs_sha256,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};
use support::workspace::{TestRepository, isolated_authority_roots, request_id};

struct EmptyProviderConfig;

impl ClosedJcs for EmptyProviderConfig {
    fn from_jcs(value: JcsValue) -> Result<Self, String> {
        match value {
            JcsValue::Object(object) if object.is_empty() => Ok(Self),
            _ => Err("test provider config must be an empty object".to_owned()),
        }
    }

    fn to_jcs(&self) -> JcsValue {
        JcsValue::Object(BTreeMap::new())
    }
}

fn decision(kind: &str, requested: bool) -> ResourceDecisionV1 {
    ResourceDecisionV1 {
        kind: kind.to_owned(),
        name: kind.to_owned(),
        state: if requested { "requested" } else { "unused" }.to_owned(),
        provider_id: (kind == "database_schema" && requested).then(|| "test-provider".to_owned()),
        reason: (!requested).then(|| "task_does_not_use_resource".to_owned()),
    }
}

fn all_decisions(requested: bool) -> Vec<ResourceDecisionV1> {
    [
        "port",
        "temp_dir",
        "build_dir",
        "database_schema",
        "log_dir",
        "cache_dir",
    ]
    .map(|kind| decision(kind, requested))
    .into()
}

fn install_provider(repo: &TestRepository, roots: &relay::workspace::authority::AuthorityRoots) {
    let executable = repo.home.join("resource-provider.mjs");
    fs::write(
        &executable,
        r#"#!/usr/bin/env node
import fs from 'node:fs';
fs.readFileSync(3, 'utf8');
const request = JSON.parse(fs.readFileSync(0, 'utf8'));
const outcomes = { create: 'allocated', inspect: 'exists', delete: 'released' };
const receipt = {
  allocation_id: request.allocation_id,
  at: '2026-07-22T12:34:56.789Z',
  operation: request.operation,
  outcome: outcomes[request.operation],
  provider_evidence_sha256: 'a'.repeat(64),
  request_id: request.request_id,
  schema: 'ProviderReceiptV1',
  value: `schema_${request.session_id.replaceAll('-', '')}`,
};
process.stdout.write(`${JSON.stringify(receipt)}\n`);
"#,
    )
    .unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
    let executable = fs::canonicalize(executable).unwrap();

    let config = repo.home.join("resource-provider-config.json");
    fs::write(&config, b"{}\n").unwrap();
    fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
    let config = fs::canonicalize(config).unwrap();
    let registry = ResourceProviderRegistryV1 {
        providers: vec![ResourceProviderRegistrationV1 {
            provider_id: "test-provider".to_owned(),
            executable_path: executable.to_string_lossy().into_owned(),
            executable_sha256: executable_sha256(&executable).unwrap(),
            config_path: config.to_string_lossy().into_owned(),
            config_sha256: jcs_sha256(&EmptyProviderConfig),
            supported_kinds: vec!["database_schema".to_owned()],
        }],
        updated_at: "2026-07-22T12:34:56.789Z".to_owned(),
    };
    atomic_create_jcs(&provider_registry_path(roots), &registry, 0o600).unwrap();
}

#[cfg(target_os = "linux")]
fn install_timeout_provider(
    repo: &TestRepository,
    roots: &relay::workspace::authority::AuthorityRoots,
    descendant_pid_path: &std::path::Path,
    daemon_fence_path: &std::path::Path,
) {
    assert!(
        [descendant_pid_path, daemon_fence_path]
            .iter()
            .all(|path| !path.to_string_lossy().contains('\'')),
        "test paths cannot be safely embedded in the provider fixture"
    );
    let executable = repo.home.join("timeout-resource-provider.py");
    fs::write(
        &executable,
        format!(
            r#"#!/usr/bin/python3
import os
import sys
import time

sys.stdin.buffer.read()
descendant = os.fork()
if descendant == 0:
    try:
        os.setsid()
        outcome = "escaped"
    except OSError as error:
        outcome = f"refused:{{error.errno}}"
    with open('{}', 'w', encoding='utf-8') as fence:
        fence.write(outcome)
    time.sleep(30)
    os._exit(0)

with open('{}', 'w', encoding='utf-8') as pid_file:
    pid_file.write(str(descendant))
os.waitpid(descendant, 0)
"#,
            daemon_fence_path.display(),
            descendant_pid_path.display(),
        ),
    )
    .unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
    let executable = fs::canonicalize(executable).unwrap();

    let config = repo.home.join("timeout-resource-provider-config.json");
    fs::write(&config, b"{}\n").unwrap();
    fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
    let config = fs::canonicalize(config).unwrap();
    let registry = ResourceProviderRegistryV1 {
        providers: vec![ResourceProviderRegistrationV1 {
            provider_id: "test-provider".to_owned(),
            executable_path: executable.to_string_lossy().into_owned(),
            executable_sha256: executable_sha256(&executable).unwrap(),
            config_path: config.to_string_lossy().into_owned(),
            config_sha256: jcs_sha256(&EmptyProviderConfig),
            supported_kinds: vec!["database_schema".to_owned()],
        }],
        updated_at: "2026-07-22T12:34:56.789Z".to_owned(),
    };
    atomic_create_jcs(&provider_registry_path(roots), &registry, 0o600).unwrap();
}

#[test]
fn resource_decisions_require_closed_six_kind_matrix() {
    let repo = TestRepository::init("resource-decision-matrix");
    let roots = isolated_authority_roots(&repo, "resource-decision-matrix");
    let repository_id = OpenedRepository::open(&repo.root)
        .unwrap()
        .identity
        .repository_id;
    let receipts = repo.home.join("must-not-create-receipts");
    let decisions = [
        "port",
        "temp_dir",
        "build_dir",
        "database_schema",
        "log_dir",
    ]
    .map(|kind| decision(kind, false));

    let error = allocate_resources(
        &roots,
        &repository_id,
        &request_id(),
        &decisions,
        &receipts,
        "2026-07-22T12:34:56.789Z",
    )
    .unwrap_err();

    assert_eq!(
        error,
        "resource decisions must contain all six kinds exactly once"
    );
    assert!(
        !receipts.exists(),
        "invalid decisions created durable receipts"
    );
    assert!(
        !roots.data.join(&repository_id).exists(),
        "invalid decisions created resource state"
    );
}

#[test]
fn all_six_resource_kinds_are_isolated_and_receipted() {
    let repo = TestRepository::init("all-resource-kinds");
    let roots = isolated_authority_roots(&repo, "all-resource-kinds");
    let repository_id = OpenedRepository::open(&repo.root)
        .unwrap()
        .identity
        .repository_id;
    install_provider(&repo, &roots);
    let decisions = all_decisions(true);
    let sessions = [request_id(), request_id()];
    let receipt_dirs = [
        repo.home.join("receipts-one"),
        repo.home.join("receipts-two"),
    ];
    let mut first = allocate_resources(
        &roots,
        &repository_id,
        &sessions[0],
        &decisions,
        &receipt_dirs[0],
        "2026-07-22T12:35:56.789Z",
    )
    .unwrap();
    let mut second = allocate_resources(
        &roots,
        &repository_id,
        &sessions[1],
        &decisions,
        &receipt_dirs[1],
        "2026-07-22T12:36:56.789Z",
    )
    .unwrap();

    for resources in [&first, &second] {
        assert_eq!(resources.decisions, decisions);
        assert_eq!(resources.allocations.len(), 6);
        assert_eq!(
            resources
                .allocations
                .iter()
                .map(|allocation| allocation.kind.as_str())
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([
                "port",
                "temp_dir",
                "build_dir",
                "database_schema",
                "log_dir",
                "cache_dir",
            ])
        );
        assert_eq!(resources.resource_fds.len(), 1);
        assert_eq!(resources.writable_paths().len(), 4);
        assert!(resources.writable_paths().iter().all(|path| path.is_dir()));
        assert_eq!(resources.inspect(&roots).unwrap().len(), 6);
    }

    let first_values = first
        .allocations
        .iter()
        .map(|allocation| (allocation.kind.clone(), allocation.value.clone()))
        .collect::<BTreeMap<_, _>>();
    let second_values = second
        .allocations
        .iter()
        .map(|allocation| (allocation.kind.clone(), allocation.value.clone()))
        .collect::<BTreeMap<_, _>>();
    for kind in first_values.keys() {
        assert_ne!(
            first_values[kind], second_values[kind],
            "{kind} allocation was shared"
        );
    }
    for address in [&first_values["port"], &second_values["port"]] {
        assert!(
            TcpListener::bind(address).is_err(),
            "held loopback port was reusable"
        );
    }
    assert!(first.environment.keys().all(|name| !name.is_empty()));
    assert!(second.environment.keys().all(|name| !name.is_empty()));

    let config = repo.home.join("resource-provider-config.json");
    fs::write(&config, b"{\"drift\":true}\n").unwrap();
    assert!(
        first
            .inspect(&roots)
            .unwrap_err()
            .contains("config digest drift")
    );
    fs::write(&config, b"{}\n").unwrap();
    assert_eq!(first.inspect(&roots).unwrap().len(), 6);

    for (resources, receipt_dir, released_at) in [
        (&mut first, &receipt_dirs[0], "2026-07-22T12:37:56.789Z"),
        (&mut second, &receipt_dirs[1], "2026-07-22T12:38:56.789Z"),
    ] {
        let released = resources.release(&roots, released_at).unwrap();
        assert_eq!(released.len(), 6);
        assert_eq!(resources.inspect(&roots).unwrap().len(), 6);
        assert_eq!(resources.release(&roots, released_at).unwrap(), released);
        assert!(resources.allocations.iter().all(|allocation| {
            allocation.state == "released"
                && allocation
                    .delete_receipt_sha256
                    .as_ref()
                    .is_some_and(|digest| digest.len() == 64)
                && allocation.released_at.as_deref() == Some(released_at)
        }));
        assert!(receipt_dir.join("resource-inventory-v1.json").is_file());
        for allocation in &resources.allocations {
            for operation in ["create", "inspect", "delete"] {
                assert!(
                    receipt_dir
                        .join(format!(
                            "{}-{operation}-receipt-v1.json",
                            allocation.allocation_id
                        ))
                        .is_file(),
                    "missing {operation} receipt for {}",
                    allocation.kind
                );
            }
        }
    }
    for address in [&first_values["port"], &second_values["port"]] {
        TcpListener::bind(address).expect("released loopback port remains held");
    }
    assert!(first.writable_paths().iter().all(|path| !path.exists()));
    assert!(second.writable_paths().iter().all(|path| !path.exists()));
}

#[cfg(target_os = "linux")]
#[test]
fn provider_timeout_fences_setsid_and_terminates_inherited_pipe_descendant() {
    let repo = TestRepository::init("provider-timeout-tree");
    let roots = isolated_authority_roots(&repo, "provider-timeout-tree");
    let repository_id = OpenedRepository::open(&repo.root)
        .unwrap()
        .identity
        .repository_id;
    let descendant_pid_path = repo.home.join("provider-descendant.pid");
    let daemon_fence_path = repo.home.join("provider-daemon-fence");
    install_timeout_provider(&repo, &roots, &descendant_pid_path, &daemon_fence_path);
    let decisions = Vec::from([
        "port",
        "temp_dir",
        "build_dir",
        "database_schema",
        "log_dir",
        "cache_dir",
    ])
    .into_iter()
    .map(|kind| decision(kind, kind == "database_schema"))
    .collect::<Vec<_>>();

    let started = Instant::now();
    let error = allocate_resources(
        &roots,
        &repository_id,
        &request_id(),
        &decisions,
        &repo.home.join("timeout-receipts"),
        "2026-07-22T12:35:56.789Z",
    )
    .unwrap_err();
    let elapsed = started.elapsed();

    assert!(
        error.contains("resource provider test-provider timed out"),
        "{error}"
    );
    assert!(
        elapsed >= Duration::from_secs(4) && elapsed < Duration::from_secs(8),
        "provider timeout returned outside its configured bound: {elapsed:?}"
    );
    assert_eq!(
        fs::read_to_string(&daemon_fence_path).unwrap(),
        format!("refused:{}", libc::EPERM),
        "provider descendant escaped its owned process group"
    );
    let descendant_pid = fs::read_to_string(&descendant_pid_path)
        .unwrap()
        .trim()
        .parse::<u32>()
        .unwrap();
    let stat = fs::read_to_string(format!("/proc/{descendant_pid}/stat"));
    assert!(
        stat.is_err()
            || stat
                .as_deref()
                .ok()
                .and_then(|value| value.rsplit_once(") "))
                .and_then(|(_, suffix)| suffix.chars().next())
                == Some('Z'),
        "provider descendant {descendant_pid} survived timeout"
    );
}
