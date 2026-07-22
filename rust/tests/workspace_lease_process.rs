pub mod support;

use relay::workspace::authority::WorkspaceLease;
use relay::workspace::capability::WorkerCapabilityV1;
#[cfg(target_os = "linux")]
use relay::workspace::custody::{
    CONTROL_FRAME_MAX, ControlEndpoint, ControlPacket, ControlPayload, PacketKind, PeerIdentity,
    Sender,
};
#[cfg(target_os = "linux")]
use relay::workspace::platform::linux::{
    DelegatedCgroup, LandlockPolicy, ProcessIdentity, WorkerLaunch, pidfd_open, probe_closed_lease,
    process_start_token, reconcile_empty_delegated_cgroup, require_ext4_fd, signal_pidfd,
    validate_bootstrap_fds, validate_pidfd_identity,
};
use relay::workspace::platform::{
    MACOS_INADMISSIBLE_BACKEND, MACOS_STOP_REASON, admit_macos_writable_custody_for_test,
};
#[cfg(target_os = "linux")]
use relay::workspace::recover_workspace_with_roots;
use relay::workspace::schema::read_jcs_file;
use relay::workspace::schema::{AbortRequestV1, PathClaimRequestV1, parse_jcs};
#[cfg(target_os = "linux")]
use relay::workspace::schema::{JcsValue, RecoverRequestV1, WorkspaceState};
use relay::workspace::{abort_workspace_with_roots, start_workspace_with_roots_and_executable};
#[cfg(target_os = "linux")]
use std::collections::BTreeMap;
use std::fs;
#[cfg(target_os = "linux")]
use std::fs::File;
#[cfg(target_os = "linux")]
use std::fs::OpenOptions;
#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;
use support::workspace::{
    TestRepository, abort_started_workspace, git_ok, git_output, git_stdout,
    handback_started_workspace, isolated_authority_roots, manifest_fields, prepare_test_workspace,
    relay_output, request_id, start_test_workspace, wait_until, write_closed_record,
};

fn relay_test_binary() -> PathBuf {
    let configured = std::env::var_os("SESSION_RELAY_TEST_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_relay")));
    assert!(
        configured.is_absolute(),
        "Relay test binary must be absolute"
    );
    let canonical = fs::canonicalize(&configured).expect("canonicalize Relay test binary");
    assert_eq!(
        configured, canonical,
        "Relay test binary must already be canonical"
    );
    let mode = fs::metadata(&canonical)
        .expect("stat Relay test binary")
        .permissions()
        .mode();
    assert_ne!(mode & 0o111, 0, "Relay test binary must be executable");
    canonical
}

fn workspace_git_shim(started: &relay::workspace::StartedWorkspace) -> PathBuf {
    started
        .worker_capability_file
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("bin/git")
}

fn broker_git_output(started: &relay::workspace::StartedWorkspace, args: &[&str]) -> Output {
    Command::new(workspace_git_shim(started))
        .args(args)
        .current_dir(&started.result.worktree_root)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("execute closed Git broker shim")
}

fn broker_git_ok(started: &relay::workspace::StartedWorkspace, args: &[&str]) -> Output {
    let output = broker_git_output(started, args);
    assert!(
        output.status.success(),
        "broker Git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn exact_worktree_index(root: &Path) -> Vec<u8> {
    let index = git_stdout(
        root,
        ["rev-parse", "--path-format=absolute", "--git-path", "index"],
    );
    fs::read(index).expect("read exact worktree index")
}

fn temp_dir_resource(started: &relay::workspace::StartedWorkspace) -> PathBuf {
    PathBuf::from(
        &started
            .resources
            .allocations
            .iter()
            .find(|allocation| allocation.kind == "temp_dir")
            .expect("temp_dir resource allocation")
            .value,
    )
}

#[cfg(target_os = "linux")]
fn isolation_child_if_requested() -> bool {
    let Ok(result_path) = std::env::var("SESSION_RELAY_ISOLATION_RESULT") else {
        return false;
    };
    let release_path = std::env::var("SESSION_RELAY_ISOLATION_RELEASE").unwrap();
    let other_root = PathBuf::from(std::env::var("SESSION_RELAY_OTHER_ROOT").unwrap());
    let other_index = PathBuf::from(std::env::var("SESSION_RELAY_OTHER_INDEX").unwrap());
    let other_ref = PathBuf::from(std::env::var("SESSION_RELAY_OTHER_REF").unwrap());
    let other_resource = PathBuf::from(std::env::var("SESSION_RELAY_OTHER_RESOURCE").unwrap());
    let denied = [
        fs::read(other_root.join("base.txt")).is_err(),
        OpenOptions::new()
            .write(true)
            .open(other_root.join("base.txt"))
            .is_err(),
        fs::read(&other_index).is_err(),
        OpenOptions::new().write(true).open(&other_index).is_err(),
        fs::read(&other_ref).is_err(),
        OpenOptions::new().write(true).open(&other_ref).is_err(),
        fs::read(other_resource.join("second-owner")).is_err()
            && fs::read(other_resource.join("first-owner")).is_err(),
        fs::write(other_resource.join("cross-worker"), b"forbidden\n").is_err(),
    ];
    fs::write(
        &result_path,
        if denied.into_iter().all(|value| value) {
            b"denied\n".as_slice()
        } else {
            b"escaped\n".as_slice()
        },
    )
    .unwrap();
    wait_until("isolation child release", Duration::from_secs(10), || {
        Path::new(&release_path).exists()
    });
    true
}

#[cfg(target_os = "linux")]
fn run_isolation_probe(
    test: &str,
    own_root: &Path,
    own_resource: &Path,
    other_root: &Path,
    other_resource: &Path,
    other_index: &Path,
    other_ref: &Path,
) {
    let result = own_root.join(format!("isolation-{}.result", request_id()));
    let release = own_root.join(format!("isolation-{}.release", request_id()));
    let cgroup = DelegatedCgroup::create(&request_id()).unwrap();
    let launch = WorkerLaunch {
        executable: std::env::current_exe().unwrap(),
        arguments: vec![
            test.to_owned(),
            "--exact".to_owned(),
            "--nocapture".to_owned(),
        ],
        environment: BTreeMap::from([
            (
                "SESSION_RELAY_ISOLATION_RESULT".to_owned(),
                result.to_string_lossy().into_owned(),
            ),
            (
                "SESSION_RELAY_ISOLATION_RELEASE".to_owned(),
                release.to_string_lossy().into_owned(),
            ),
            (
                "SESSION_RELAY_OTHER_ROOT".to_owned(),
                other_root.to_string_lossy().into_owned(),
            ),
            (
                "SESSION_RELAY_OTHER_RESOURCE".to_owned(),
                other_resource.to_string_lossy().into_owned(),
            ),
            (
                "SESSION_RELAY_OTHER_INDEX".to_owned(),
                other_index.to_string_lossy().into_owned(),
            ),
            (
                "SESSION_RELAY_OTHER_REF".to_owned(),
                other_ref.to_string_lossy().into_owned(),
            ),
        ]),
        cwd: own_root.to_owned(),
        resource_fds: Vec::new(),
        sandbox: LandlockPolicy {
            workspace: own_root.to_owned(),
            readable: Vec::new(),
            executable_runtime: Vec::new(),
            pinned_readable: Vec::new(),
            writable_resources: vec![own_resource.to_owned()],
        },
    };
    let prepared = cgroup.launch_worker(&launch).unwrap();
    let verified = prepared.verify_activation().unwrap();
    let (identity, _) = verified.release_after_ack(|_| Ok(())).unwrap();
    wait_until("workspace isolation probe", Duration::from_secs(5), || {
        result.exists()
    });
    assert_eq!(fs::read(&result).unwrap(), b"denied\n");
    let empty = cgroup.kill_and_wait_empty(&identity).unwrap();
    assert!(!empty.populated);
    cgroup.remove().unwrap();
    fs::remove_file(result).unwrap();
}

#[cfg(target_os = "linux")]
fn git_bypass_child_if_requested() -> bool {
    let Ok(result_path) = std::env::var("SESSION_RELAY_GIT_BYPASS_RESULT") else {
        return false;
    };
    let release_path = std::env::var("SESSION_RELAY_GIT_BYPASS_RELEASE").unwrap();
    let result = match Command::new("/usr/bin/git")
        .args(["add", "--", "base.txt"])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
    {
        Ok(output) => format!(
            "{}\n{}",
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr)
        ),
        Err(error) => format!("spawn-error\n{error}"),
    };
    fs::write(&result_path, result).unwrap();
    wait_until("Git bypass child release", Duration::from_secs(10), || {
        Path::new(&release_path).exists()
    });
    true
}
fn workspace_start_child_if_requested() -> bool {
    let Ok(result_path) = std::env::var("SESSION_RELAY_START_CHILD_RESULT") else {
        return false;
    };
    let go = PathBuf::from(std::env::var("SESSION_RELAY_START_CHILD_GO").unwrap());
    let release = PathBuf::from(std::env::var("SESSION_RELAY_START_CHILD_RELEASE").unwrap());
    let roots = relay::workspace::authority::AuthorityRoots {
        authority: PathBuf::from(std::env::var("SESSION_RELAY_START_CHILD_AUTHORITY").unwrap()),
        data: PathBuf::from(std::env::var("SESSION_RELAY_START_CHILD_DATA").unwrap()),
        euid: unsafe { libc::geteuid() },
    };
    let request_file = PathBuf::from(std::env::var("SESSION_RELAY_START_CHILD_REQUEST").unwrap());
    let request_sha256 = std::env::var("SESSION_RELAY_START_CHILD_REQUEST_SHA").unwrap();
    let coordinator =
        PathBuf::from(std::env::var("SESSION_RELAY_START_CHILD_COORDINATOR").unwrap());
    let relay = PathBuf::from(std::env::var("SESSION_RELAY_START_CHILD_RELAY").unwrap());
    fs::write(format!("{result_path}.ready"), b"ready\n").unwrap();
    wait_until(
        "workspace start race barrier",
        Duration::from_secs(10),
        || go.exists(),
    );
    match start_workspace_with_roots_and_executable(
        &roots,
        &request_file,
        &request_sha256,
        Some(&coordinator),
        &relay,
    ) {
        Ok(started) => {
            let (state, journal_head, worktree_root) = manifest_fields(&started);
            fs::write(
                &result_path,
                format!(
                    "ok:{}:{}:{}",
                    started.result.session_id,
                    state.as_str(),
                    worktree_root
                ),
            )
            .unwrap();
            wait_until(
                "workspace race winner release",
                Duration::from_secs(20),
                || release.exists(),
            );
            let expected_head = git_stdout(Path::new(&worktree_root), ["rev-parse", "HEAD"]);
            let abort = AbortRequestV1 {
                request_id: request_id(),
                repository_path: std::env::var("SESSION_RELAY_START_CHILD_REPOSITORY").unwrap(),
                repository_id: started.result.repository_id.clone(),
                session_id: started.result.session_id.clone(),
                expected_state: state,
                expected_journal_head_sha256: journal_head,
                expected_head,
                reason: "workspace start race cleanup".to_owned(),
                created_at: "2026-07-22T12:59:56.789Z".to_owned(),
            };
            let abort_file = PathBuf::from(format!("{result_path}.abort-request-v1.json"));
            let abort_sha256 = write_closed_record(&abort_file, &abort);
            drop(started.lease);
            let cleanup =
                abort_workspace_with_roots(&roots, &abort_file, &abort_sha256, &coordinator)
                    .unwrap();
            assert!(cleanup.lease_released);
            assert!(cleanup.worktree_removed);
            assert!(cleanup.branch_removed);
            fs::write(format!("{result_path}.clean"), b"clean\n").unwrap();
        }
        Err(error) => fs::write(&result_path, format!("error:{error}")).unwrap(),
    }
    true
}

fn prepare_workspace_start_race(
    repository: &TestRepository,
    roots: &relay::workspace::authority::AuthorityRoots,
    coordinator_capability_file: &Path,
) -> (PathBuf, String, String, PathBuf) {
    let prepared = prepare_test_workspace(
        repository,
        roots,
        "same-worktree-race",
        "commit",
        vec![PathClaimRequestV1 {
            path: "base.txt".to_owned(),
            path_type: "file".to_owned(),
            mode: "exclusive".to_owned(),
        }],
    );
    assert!(coordinator_capability_file.is_file());
    (
        prepared.request_file,
        prepared.request_sha256,
        prepared.request.request_id,
        prepared.relay_executable,
    )
}

#[test]
fn two_writers_same_worktree_exactly_one_lease() {
    if workspace_start_child_if_requested() {
        return;
    }
    if !cfg!(target_os = "linux") {
        eprintln!("SKIP native-only: writable workspace custody requires Linux");
        return;
    }
    let repository = TestRepository::init("same-worktree-process-race");
    let roots = isolated_authority_roots(&repository, "same-worktree-process-race");
    let seed = start_test_workspace(
        &repository,
        &roots,
        "authority-seed",
        vec![PathClaimRequestV1 {
            path: "base.txt".to_owned(),
            path_type: "file".to_owned(),
            mode: "exclusive".to_owned(),
        }],
        None,
    );
    let coordinator = PathBuf::from(&seed.result.coordinator_capability_file);
    let seed_cleanup =
        abort_started_workspace(&repository, &roots, seed, "prepare start race authority");
    assert!(seed_cleanup.lease_released);
    let (request_file, request_sha256, session_id, relay) =
        prepare_workspace_start_race(&repository, &roots, &coordinator);
    let before = repository.snapshot();
    let go = repository.home.join("same-worktree-race.go");
    let release = repository.home.join("same-worktree-race.release");
    let results = [
        repository.home.join("same-worktree-one.result"),
        repository.home.join("same-worktree-two.result"),
    ];
    let children = results.clone().map(|result| {
        Command::new(std::env::current_exe().unwrap())
            .args([
                "two_writers_same_worktree_exactly_one_lease",
                "--exact",
                "--nocapture",
            ])
            .env("SESSION_RELAY_START_CHILD_RESULT", &result)
            .env("SESSION_RELAY_START_CHILD_GO", &go)
            .env("SESSION_RELAY_START_CHILD_RELEASE", &release)
            .env("SESSION_RELAY_START_CHILD_AUTHORITY", &roots.authority)
            .env("SESSION_RELAY_START_CHILD_DATA", &roots.data)
            .env("SESSION_RELAY_START_CHILD_REQUEST", &request_file)
            .env("SESSION_RELAY_START_CHILD_REQUEST_SHA", &request_sha256)
            .env("SESSION_RELAY_START_CHILD_COORDINATOR", &coordinator)
            .env("SESSION_RELAY_START_CHILD_RELAY", &relay)
            .env("SESSION_RELAY_START_CHILD_REPOSITORY", &repository.root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    });
    wait_until(
        "both real workspace starters",
        Duration::from_secs(10),
        || {
            results
                .iter()
                .all(|path| PathBuf::from(format!("{}.ready", path.display())).exists())
        },
    );
    fs::write(&go, b"go\n").unwrap();
    wait_until(
        "workspace start race outcomes",
        Duration::from_secs(20),
        || results.iter().all(|path| path.exists()),
    );
    let outcomes = results
        .iter()
        .map(|path| fs::read_to_string(path).unwrap())
        .collect::<Vec<_>>();
    let winner = outcomes
        .iter()
        .find(|outcome| outcome.starts_with("ok:"))
        .expect("one real start winner");
    assert!(
        winner.starts_with(&format!("ok:{session_id}:Running:")),
        "winner did not cross durable LeaseHeld into Running: {winner}"
    );
    let loser = outcomes
        .iter()
        .find(|outcome| outcome.starts_with("error:"))
        .expect("one real start loser");
    assert_eq!(
        loser,
        &format!(
            "error:Workspace already owned by session {session_id}. Open a separate worktree or continue in read-only mode."
        )
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| outcome.starts_with("ok:"))
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| outcome.starts_with("error:"))
            .count(),
        1
    );
    let live_source = repository.snapshot();
    assert_eq!(live_source.head, before.head);
    assert_eq!(live_source.status, before.status);
    assert_eq!(live_source.index, before.index);
    assert_eq!(live_source.tree, before.tree);
    let winner_root = PathBuf::from(winner.rsplit_once(':').unwrap().1);
    assert!(winner_root.is_dir());
    let session_dir = roots
        .authority
        .join("repositories")
        .join(
            parse_jcs(&fs::read(&coordinator).unwrap(), true)
                .unwrap()
                .object()
                .unwrap()["repository_id"]
                .as_str()
                .unwrap(),
        )
        .join("sessions")
        .join(&session_id);
    assert_eq!(
        parse_jcs(
            &fs::read(session_dir.join("manifest-v1.json")).unwrap(),
            true
        )
        .unwrap()
        .object()
        .unwrap()["state"]
            .as_str()
            .unwrap(),
        "Running"
    );
    assert!(session_dir.join("lease-identity-v1.json").is_file());
    assert!(
        fs::read_dir(session_dir.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .all(|entry| !entry.file_name().to_string_lossy().starts_with('.')),
        "losing start left a partial authority publication"
    );

    fs::write(&release, b"release\n").unwrap();
    wait_until("workspace race cleanup", Duration::from_secs(20), || {
        results
            .iter()
            .any(|path| PathBuf::from(format!("{}.clean", path.display())).exists())
    });
    for child in children {
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(!winner_root.exists());
    assert_eq!(repository.snapshot(), before);
}

#[test]
fn separate_worktrees_both_hold_leases() {
    if !cfg!(target_os = "linux") {
        eprintln!("SKIP native-only: writable workspace custody requires Linux");
        return;
    }
    #[cfg(target_os = "linux")]
    if isolation_child_if_requested() {
        return;
    }
    let mut repository = TestRepository::init("separate-worktree-leases");
    fs::write(repository.root.join("first.txt"), b"first base\n").unwrap();
    fs::write(repository.root.join("second.txt"), b"second base\n").unwrap();
    git_ok(&repository.root, ["add", "--", "first.txt", "second.txt"]);
    git_ok(
        &repository.root,
        ["commit", "--quiet", "-m", "claim fixtures"],
    );
    repository.base_commit = git_stdout(&repository.root, ["rev-parse", "HEAD"]);
    let roots = isolated_authority_roots(&repository, "separate-worktree-leases");
    let first = start_test_workspace(
        &repository,
        &roots,
        "first-worker",
        vec![PathClaimRequestV1 {
            path: "first.txt".to_owned(),
            path_type: "file".to_owned(),
            mode: "exclusive".to_owned(),
        }],
        None,
    );
    let second = start_test_workspace(
        &repository,
        &roots,
        "second-worker",
        vec![PathClaimRequestV1 {
            path: "second.txt".to_owned(),
            path_type: "file".to_owned(),
            mode: "exclusive".to_owned(),
        }],
        Some(Path::new(&first.result.coordinator_capability_file)),
    );

    let first_root = PathBuf::from(&first.result.worktree_root);
    let second_root = PathBuf::from(&second.result.worktree_root);
    assert_eq!(
        manifest_fields(&first).0,
        relay::workspace::schema::WorkspaceState::Running
    );
    assert_eq!(
        manifest_fields(&second).0,
        relay::workspace::schema::WorkspaceState::Running
    );
    assert_ne!(first_root, second_root);
    assert_ne!(first.result.branch_ref, second.result.branch_ref);
    assert_ne!(first.result.session_id, second.result.session_id);
    assert!(WorkspaceLease::acquire(first.lease.path()).is_err());
    assert!(WorkspaceLease::acquire(second.lease.path()).is_err());

    let first_resource = temp_dir_resource(&first);
    let second_resource = temp_dir_resource(&second);
    assert_ne!(first_resource, second_resource);
    fs::write(first_resource.join("first-owner"), b"first resource\n").unwrap();
    fs::write(second_resource.join("second-owner"), b"second resource\n").unwrap();
    assert!(!first_resource.join("second-owner").exists());
    assert!(!second_resource.join("first-owner").exists());
    assert!(
        first
            .manifest_file
            .with_file_name("custody-active-v1.json")
            .is_file()
    );
    assert!(
        second
            .manifest_file
            .with_file_name("custody-active-v1.json")
            .is_file()
    );

    let first_index_before = exact_worktree_index(&first_root);
    let second_index_before = exact_worktree_index(&second_root);
    let first_ref_before = git_stdout(&first_root, ["rev-parse", &first.result.branch_ref]);
    #[cfg(target_os = "linux")]
    {
        let first_index_path = PathBuf::from(git_stdout(
            &first_root,
            ["rev-parse", "--path-format=absolute", "--git-path", "index"],
        ));
        let second_index_path = PathBuf::from(git_stdout(
            &second_root,
            ["rev-parse", "--path-format=absolute", "--git-path", "index"],
        ));
        let first_ref_path = PathBuf::from(git_stdout(
            &first_root,
            [
                "rev-parse",
                "--path-format=absolute",
                "--git-path",
                &first.result.branch_ref,
            ],
        ));
        let second_ref_path = PathBuf::from(git_stdout(
            &second_root,
            [
                "rev-parse",
                "--path-format=absolute",
                "--git-path",
                &second.result.branch_ref,
            ],
        ));
        run_isolation_probe(
            "separate_worktrees_both_hold_leases",
            &first_root,
            &first_resource,
            &second_root,
            &second_resource,
            &second_index_path,
            &second_ref_path,
        );
        run_isolation_probe(
            "separate_worktrees_both_hold_leases",
            &second_root,
            &second_resource,
            &first_root,
            &first_resource,
            &first_index_path,
            &first_ref_path,
        );
    }
    let second_ref_before = git_stdout(&second_root, ["rev-parse", &second.result.branch_ref]);

    fs::write(first_root.join("first.txt"), b"first worker\n").unwrap();
    broker_git_ok(&first, &["add", "--", "first.txt"]);
    let first_commit_output = broker_git_ok(&first, &["commit", "-m", "first claimed commit"]);
    let first_commit = String::from_utf8(first_commit_output.stdout)
        .unwrap()
        .trim()
        .to_owned();
    assert_eq!(git_stdout(&first_root, ["rev-parse", "HEAD"]), first_commit);
    assert_ne!(first_commit, first_ref_before);
    assert_eq!(
        git_stdout(&second_root, ["rev-parse", &second.result.branch_ref]),
        second_ref_before
    );
    assert_eq!(exact_worktree_index(&second_root), second_index_before);
    assert_eq!(
        fs::read(second_root.join("first.txt")).unwrap(),
        b"first base\n"
    );
    assert!(!second_root.join("first-owner").exists());

    let first_index_after_commit = exact_worktree_index(&first_root);
    fs::write(second_root.join("second.txt"), b"second worker\n").unwrap();
    broker_git_ok(&second, &["add", "--", "second.txt"]);
    let second_commit_output = broker_git_ok(&second, &["commit", "-m", "second claimed commit"]);
    let second_commit = String::from_utf8(second_commit_output.stdout)
        .unwrap()
        .trim()
        .to_owned();
    assert_eq!(
        git_stdout(&second_root, ["rev-parse", "HEAD"]),
        second_commit
    );
    assert_ne!(second_commit, second_ref_before);
    assert_eq!(
        git_stdout(&first_root, ["rev-parse", &first.result.branch_ref]),
        first_commit
    );
    assert_eq!(exact_worktree_index(&first_root), first_index_after_commit);
    assert_eq!(
        fs::read(first_root.join("second.txt")).unwrap(),
        b"second base\n"
    );
    assert!(!first_root.join("second-owner").exists());
    assert_ne!(exact_worktree_index(&first_root), first_index_before);

    handback_started_workspace(&repository, &first);
    handback_started_workspace(&repository, &second);
    assert_eq!(
        manifest_fields(&first).0,
        relay::workspace::schema::WorkspaceState::HandbackReady
    );
    assert_eq!(
        manifest_fields(&second).0,
        relay::workspace::schema::WorkspaceState::HandbackReady
    );
    assert!(
        first
            .manifest_file
            .with_file_name("handback-receipt-v1.json")
            .is_file()
    );
    assert!(
        second
            .manifest_file
            .with_file_name("handback-receipt-v1.json")
            .is_file()
    );
    assert!(WorkspaceLease::acquire(first.lease.path()).is_err());
    assert!(WorkspaceLease::acquire(second.lease.path()).is_err());

    let first_lease_path = first.lease.path().to_owned();
    let second_lease_path = second.lease.path().to_owned();
    let first_cleanup = abort_started_workspace(&repository, &roots, first, "test cleanup");
    assert!(first_cleanup.lease_released);
    let independent_first = WorkspaceLease::acquire(&first_lease_path)
        .expect("first lifetime lease releases independently");
    drop(independent_first);
    assert!(
        WorkspaceLease::acquire(&second_lease_path).is_err(),
        "first cleanup released the second lifetime lease"
    );
    assert!(second_root.is_dir());
    assert!(second_resource.is_dir());
    assert_eq!(
        fs::read(second_resource.join("second-owner")).unwrap(),
        b"second resource\n"
    );

    let second_cleanup = abort_started_workspace(&repository, &roots, second, "test cleanup");
    assert!(second_cleanup.lease_released);
    assert!(!second_resource.exists());
    assert!(!first_resource.exists());
}

#[test]
fn read_only_spawn_coexists_with_writer() {
    if !cfg!(target_os = "linux") {
        eprintln!("SKIP native-only: writable workspace custody requires Linux");
        return;
    }
    let repository = TestRepository::init("read-only-coexists");
    let roots = isolated_authority_roots(&repository, "read-only-coexists");
    let writer = start_test_workspace(
        &repository,
        &roots,
        "writer",
        vec![PathClaimRequestV1 {
            path: "base.txt".to_owned(),
            path_type: "file".to_owned(),
            mode: "exclusive".to_owned(),
        }],
        None,
    );
    let worktree = PathBuf::from(&writer.result.worktree_root);
    let observation = relay_output(
        &worktree,
        [
            "spawn",
            writer.result.worktree_root.as_str(),
            "--read-only",
            "--tool",
            "claude",
            "--reply-to",
            "observer",
            "--dry",
            "--",
            "inspect without mutation",
        ],
    );
    assert!(
        observation.status.success(),
        "{}",
        String::from_utf8_lossy(&observation.stderr)
    );
    let plan = String::from_utf8(observation.stdout).unwrap();
    assert!(
        plan.contains("\"args\":["),
        "dry spawn omitted args: {plan}"
    );
    assert!(
        plan.contains("\"--permission-mode\"") && plan.contains("\"plan\""),
        "dry spawn did not project Claude read-only arguments: {plan}"
    );

    let spawn_home = repository.home.join("read-only-spawn-home");
    fs::create_dir(&spawn_home).unwrap();
    let fixture = repository.home.join("read-only-claude-fixture");
    let evidence = repository.home.join("read-only-evidence");
    let evidence_stderr = repository.home.join("read-only-evidence.stderr");
    fs::write(
        &fixture,
        r#"#!/bin/sh
session=
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--session-id" ]; then session="$2"; shift 2; continue; fi
  shift
done
test -n "$session" || exit 91
printf '{"session_id":"%s","cwd":"%s","source":"startup"}' "$session" "$PWD" |
  "$RELAY_TEST_BIN" hook >/dev/null || exit 92
git reset --hard HEAD > /dev/null 2>"$RELAY_TEST_EVIDENCE.stderr"
status=$?
printf '%s\n' "$status" >"$RELAY_TEST_EVIDENCE"
exit 0
"#,
    )
    .unwrap();
    fs::set_permissions(&fixture, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(worktree.join("base.txt"), b"writer dirty bytes\n").unwrap();
    let shim_dir = workspace_git_shim(&writer).parent().unwrap().to_owned();
    let relay = relay_test_binary();
    let launched = Command::new(&relay)
        .args([
            "spawn",
            writer.result.worktree_root.as_str(),
            "--read-only",
            "--tool",
            "claude",
            "--reply-to",
            "observer",
            "--timeout",
            "5",
            "--",
            "attempt a forbidden worktree mutation",
        ])
        .current_dir(&worktree)
        .env("AGENT_RELAY_HOME", &spawn_home)
        .env("RELAY_SPAWN_CMD_CLAUDE", &fixture)
        .env("RELAY_TEST_BIN", &relay)
        .env("RELAY_TEST_EVIDENCE", &evidence)
        .env("PATH", format!("{}:/usr/bin:/bin", shim_dir.display()))
        .output()
        .unwrap();
    assert!(
        launched.status.success(),
        "real read-only spawn failed: {}{}",
        String::from_utf8_lossy(&launched.stdout),
        String::from_utf8_lossy(&launched.stderr)
    );
    wait_until(
        "read-only fixture mutation result",
        Duration::from_secs(5),
        || evidence.exists() && evidence_stderr.exists(),
    );
    assert_ne!(
        fs::read_to_string(&evidence).unwrap().trim(),
        "0",
        "Relay-launched read-only fixture mutated through Git"
    );
    assert!(
        fs::read_to_string(&evidence_stderr)
            .unwrap()
            .contains("Git operation reset is refused by the closed broker")
    );
    assert_eq!(
        fs::read(worktree.join("base.txt")).unwrap(),
        b"writer dirty bytes\n"
    );
    assert!(WorkspaceLease::acquire(writer.lease.path()).is_err());
    fs::write(worktree.join("base.txt"), b"base\n").unwrap();
    let cleanup = abort_started_workspace(&repository, &roots, writer, "test cleanup");
    assert!(cleanup.lease_released);
}

#[test]
fn worker_merge_rebase_reset_and_force_push_are_refused() {
    if !cfg!(target_os = "linux") {
        eprintln!("SKIP native-only: writable workspace custody requires Linux");
        return;
    }
    #[cfg(target_os = "linux")]
    if git_bypass_child_if_requested() {
        return;
    }

    let repository = TestRepository::init("forbidden-worker-git");
    let roots = isolated_authority_roots(&repository, "forbidden-worker-git");
    let writer = start_test_workspace(
        &repository,
        &roots,
        "writer",
        vec![PathClaimRequestV1 {
            path: "base.txt".to_owned(),
            path_type: "file".to_owned(),
            mode: "exclusive".to_owned(),
        }],
        None,
    );
    let worktree = PathBuf::from(&writer.result.worktree_root);
    let capability: WorkerCapabilityV1 =
        read_jcs_file(&writer.worker_capability_file, None).unwrap();
    let broker_socket = PathBuf::from(capability.broker_socket);
    let broker_close = writer.manifest_file.with_file_name("broker-close-v1.json");

    fs::write(worktree.join("base.txt"), b"owned commit\n").unwrap();
    broker_git_ok(&writer, &["add", "--", "base.txt"]);
    broker_git_ok(&writer, &["restore", "--staged", "--", "base.txt"]);
    assert!(
        git_output(&worktree, ["diff", "--cached", "--quiet"])
            .status
            .success()
    );
    broker_git_ok(&writer, &["add", "--", "base.txt"]);
    broker_git_ok(&writer, &["rm", "--cached", "--", "base.txt"]);
    broker_git_ok(&writer, &["restore", "--staged", "--", "base.txt"]);
    broker_git_ok(&writer, &["add", "--", "base.txt"]);
    let pre_commit = git_stdout(&worktree, ["rev-parse", "HEAD"]);
    let commit = broker_git_ok(&writer, &["commit", "-m", "owned broker commit"]);
    let commit = String::from_utf8(commit.stdout).unwrap().trim().to_owned();
    assert_ne!(commit, pre_commit);
    assert_eq!(git_stdout(&worktree, ["rev-parse", "HEAD"]), commit);
    assert_eq!(
        fs::read(worktree.join("base.txt")).unwrap(),
        b"owned commit\n"
    );

    let forbidden_mutators: &[&[&str]] = &[
        &["am", "--abort"],
        &["apply", "/dev/null"],
        &["bisect", "start"],
        &["branch", "forbidden"],
        &["bundle", "create", "forbidden.bundle", "HEAD"],
        &["checkout", "-B", "forbidden"],
        &["cherry-pick", "HEAD"],
        &["clean", "-fd"],
        &["clone", ".", "forbidden-clone"],
        &["commit-tree", "HEAD^{tree}"],
        &["config", "user.name", "forbidden"],
        &["fetch", "origin"],
        &["gc"],
        &["init"],
        &["maintenance", "run"],
        &["merge", "HEAD"],
        &["merge-file", "base.txt", "base.txt", "base.txt"],
        &["mv", "base.txt", "moved.txt"],
        &["notes", "add", "-m", "forbidden"],
        &["pack-refs", "--all"],
        &["pull", "--ff-only"],
        &["push", "--force", "origin", "HEAD"],
        &["rebase", "HEAD"],
        &["remote", "add", "forbidden", "."],
        &["replace", "HEAD", "HEAD"],
        &["reset", "--hard", "HEAD"],
        &["revert", "HEAD"],
        &["sparse-checkout", "set", "base.txt"],
        &["stash", "push"],
        &["submodule", "update", "--init"],
        &["switch", "-c", "forbidden"],
        &["tag", "forbidden"],
        &["update-index", "--assume-unchanged", "base.txt"],
        &["update-ref", "refs/heads/forbidden", "HEAD"],
        &["worktree", "add", "../forbidden-worktree"],
    ];
    let closed_read_operations: &[&[&str]] = &[
        &["diff"],
        &["log", "-1"],
        &["rev-parse", "HEAD"],
        &["show", "HEAD"],
        &["status", "--short"],
    ];
    let ref_before_refusals = git_stdout(&worktree, ["rev-parse", &writer.result.branch_ref]);
    let index_before_refusals = exact_worktree_index(&worktree);
    let bytes_before_refusals = fs::read(worktree.join("base.txt")).unwrap();
    for args in forbidden_mutators.iter().chain(closed_read_operations) {
        let output = broker_git_output(&writer, args);
        assert!(
            !output.status.success(),
            "closed broker unexpectedly accepted {args:?}"
        );
        let operation = args[0];
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(&format!(
                "Git operation {operation} is refused by the closed broker"
            )),
            "wrong refusal for {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    for args in [
        &["add", "--all"][..],
        &["rm", "--", "base.txt"][..],
        &["restore", "--", "base.txt"][..],
        &["commit", "--amend"][..],
    ] {
        let output = broker_git_output(&writer, args);
        assert!(
            !output.status.success(),
            "closed broker accepted invalid allowed-operation grammar {args:?}"
        );
    }
    assert_eq!(
        git_stdout(&worktree, ["rev-parse", &writer.result.branch_ref]),
        ref_before_refusals
    );
    assert_eq!(exact_worktree_index(&worktree), index_before_refusals);
    assert_eq!(
        fs::read(worktree.join("base.txt")).unwrap(),
        bytes_before_refusals
    );

    #[cfg(target_os = "linux")]
    {
        fs::write(worktree.join("base.txt"), b"absolute Git bypass\n").unwrap();
        let bypass_index_before = exact_worktree_index(&worktree);
        let bypass_ref_before = git_stdout(&worktree, ["rev-parse", &writer.result.branch_ref]);
        let bypass_result = worktree.join("absolute-git-bypass.result");
        let bypass_release = worktree.join("absolute-git-bypass.release");
        let cgroup = DelegatedCgroup::create(&request_id())
            .expect("native runner must provide an owned delegated cgroup-v2 root");
        let executable = std::env::current_exe().unwrap();
        let pinned_readable = ["/etc/gitconfig", "/usr/share/git-core"]
            .into_iter()
            .map(PathBuf::from)
            .filter(|path| path.is_file())
            .collect();
        let launch = WorkerLaunch {
            executable,
            arguments: vec![
                "worker_merge_rebase_reset_and_force_push_are_refused".to_owned(),
                "--exact".to_owned(),
                "--nocapture".to_owned(),
            ],
            environment: BTreeMap::from([
                (
                    "SESSION_RELAY_GIT_BYPASS_RESULT".to_owned(),
                    bypass_result.to_string_lossy().into_owned(),
                ),
                (
                    "SESSION_RELAY_GIT_BYPASS_RELEASE".to_owned(),
                    bypass_release.to_string_lossy().into_owned(),
                ),
            ]),
            cwd: worktree.clone(),
            resource_fds: Vec::new(),
            sandbox: LandlockPolicy {
                workspace: worktree.clone(),
                readable: Vec::new(),
                executable_runtime: vec![PathBuf::from("/usr/bin/git")],
                pinned_readable,
                writable_resources: Vec::new(),
            },
        };
        let prepared = cgroup
            .launch_worker(&launch)
            .expect("prepare absolute-Git bypass worker");
        let verified = prepared
            .verify_activation()
            .expect("activate absolute-Git bypass worker");
        let (identity, _) = verified.release_after_ack(|_| Ok(())).unwrap();
        wait_until("absolute Git bypass result", Duration::from_secs(5), || {
            bypass_result.exists()
        });
        let bypass = fs::read_to_string(&bypass_result).unwrap();
        assert_ne!(
            bypass.lines().next().unwrap(),
            "0",
            "absolute /usr/bin/git bypass mutated the worker index"
        );
        assert!(
            bypass.contains("Permission denied") || bypass.contains("Operation not permitted"),
            "absolute Git bypass was not denied by worker custody: {bypass}"
        );
        assert_eq!(exact_worktree_index(&worktree), bypass_index_before);
        assert_eq!(
            git_stdout(&worktree, ["rev-parse", &writer.result.branch_ref]),
            bypass_ref_before
        );
        let empty = cgroup.kill_and_wait_empty(&identity).unwrap();
        assert!(!empty.populated);
        cgroup.remove().unwrap();
        fs::write(worktree.join("base.txt"), b"owned commit\n").unwrap();
        fs::remove_file(&bypass_result).unwrap();
    }

    assert!(WorkspaceLease::acquire(writer.lease.path()).is_err());
    let cleanup = abort_started_workspace(&repository, &roots, writer, "test cleanup");
    assert!(cleanup.capabilities_revoked);
    assert!(!broker_socket.exists());
    assert!(broker_close.is_file());
}

#[cfg(target_os = "linux")]
fn duplicate(fd: RawFd) -> OwnedFd {
    let copied = unsafe { libc::dup(fd) };
    assert!(
        copied >= 0,
        "dup failed: {}",
        std::io::Error::last_os_error()
    );
    unsafe { OwnedFd::from_raw_fd(copied) }
}

#[cfg(target_os = "linux")]
fn raw_send(fd: RawFd, bytes: &[u8]) {
    let sent = unsafe { libc::send(fd, bytes.as_ptr().cast(), bytes.len(), 0) };
    assert_eq!(
        sent,
        bytes.len() as isize,
        "send failed: {}",
        std::io::Error::last_os_error()
    );
}

#[test]
fn custody_packets_reject_malformed_frames_replay_and_wrong_fds() {
    #[cfg(not(target_os = "linux"))]
    {
        assert_eq!(
            admit_macos_writable_custody_for_test().unwrap_err(),
            MACOS_STOP_REASON,
        );
        return;
    }
    #[cfg(target_os = "linux")]
    {
        let key = [0x5a; 32];
        for malformed in [
            Vec::new(),
            b"{}\n".to_vec(),
            br#"{"generation":1,"kind":"HEARTBEAT","mac":"0000000000000000000000000000000000000000000000000000000000000000","payload":{},"sender":"guardian","seq":1,"session_id":"11111111-1111-4111-8111-111111111111","v":1,"x":null}"#.to_vec(),
            br#"{"generation":1,"kind":"HEARTBEAT","mac":"0000000000000000000000000000000000000000000000000000000000000000","payload":{},"sender":"guardian","seq":1,"session_id":"11111111-1111-4111-8111-111111111111","v":1}"#.to_vec(),
        ] {
            assert!(ControlPacket::decode(&malformed, &key).is_err());
        }

        let session = "11111111-1111-4111-8111-111111111111".to_owned();
        let peer = PeerIdentity::current().unwrap();
        let (guardian_fd, supervisor_fd) = ControlEndpoint::pair().unwrap();
        let raw_guardian = duplicate(guardian_fd.as_raw_fd());
        let mut guardian = ControlEndpoint::new(
            guardian_fd,
            key,
            session.clone(),
            1,
            Sender::Guardian,
            peer.clone(),
        )
        .unwrap();
        let mut supervisor =
            ControlEndpoint::new(supervisor_fd, key, session, 1, Sender::Supervisor, peer).unwrap();

        guardian
            .send(PacketKind::GuardianReady, ControlPayload::new(), &[])
            .unwrap();
        let first = supervisor.receive(Duration::from_secs(1), 0).unwrap();
        assert_eq!(first.packet.kind, PacketKind::GuardianReady);
        let exact = first.packet.encode().unwrap();
        raw_send(raw_guardian.as_raw_fd(), &exact);
        assert!(
            supervisor
                .receive(Duration::from_secs(1), 0)
                .unwrap_err()
                .contains("sequence")
        );

        let transferred = File::open("/dev/null").unwrap();
        guardian
            .send(
                PacketKind::WorkerPrepared,
                ControlPayload::new(),
                &[transferred.as_raw_fd()],
            )
            .unwrap();
        assert_eq!(
            supervisor.receive(Duration::from_secs(1), 0).unwrap_err(),
            "custody packet has 1 FDs; expected one of [0]"
        );

        let wrong_types: [OwnedFd; 4] =
            std::array::from_fn(|_| File::open("/dev/null").unwrap().into());
        assert_eq!(
            validate_bootstrap_fds(&wrong_types).unwrap_err(),
            "BOOTSTRAP cgroup FD access modes are not exact"
        );

        let trunc_peer = PeerIdentity::current().unwrap();
        let (raw_truncated, truncated_receiver) = ControlEndpoint::pair().unwrap();
        let mut truncated_receiver = ControlEndpoint::new(
            truncated_receiver,
            key,
            "31111111-1111-4111-8111-111111111111".to_owned(),
            1,
            Sender::Supervisor,
            trunc_peer,
        )
        .unwrap();
        raw_send(
            raw_truncated.as_raw_fd(),
            &vec![b'x'; CONTROL_FRAME_MAX + 1],
        );
        assert!(
            truncated_receiver
                .receive(Duration::from_secs(1), 0)
                .unwrap_err()
                .contains("truncated")
        );

        let expected_parent = PeerIdentity::current().unwrap();
        let (child_fd, parent_fd) = ControlEndpoint::pair().unwrap();
        let mut parent = ControlEndpoint::new(
            parent_fd,
            key,
            "21111111-1111-4111-8111-111111111111".to_owned(),
            1,
            Sender::Supervisor,
            expected_parent,
        )
        .unwrap();
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed: {}", std::io::Error::last_os_error());
        if pid == 0 {
            let bytes = b"{}";
            unsafe {
                libc::send(child_fd.as_raw_fd(), bytes.as_ptr().cast(), bytes.len(), 0);
                libc::_exit(0);
            }
        }
        drop(child_fd);
        assert!(
            parent
                .receive(Duration::from_secs(1), 0)
                .unwrap_err()
                .contains("credentials changed")
        );
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
        assert!(libc::WIFEXITED(status));
    }
}

#[cfg(target_os = "linux")]
fn hostile_child_if_requested() -> bool {
    let Some(ready) = std::env::var_os("SESSION_RELAY_HOSTILE_READY") else {
        return false;
    };
    let first = unsafe { libc::fork() };
    if first == 0 {
        unsafe {
            libc::setsid();
        }
        let grandchild = unsafe { libc::fork() };
        if grandchild == 0 {
            fs::write(ready, b"ready\n").unwrap();
        }
        loop {
            unsafe {
                libc::pause();
            }
        }
    }
    loop {
        unsafe {
            libc::pause();
        }
    }
}

#[cfg(target_os = "linux")]
fn recorded_custodian(active_path: &Path, actor: &str) -> ProcessIdentity {
    let active = parse_jcs(&fs::read(active_path).unwrap(), true)
        .unwrap()
        .object()
        .unwrap();
    let pid = active[&format!("{actor}_pid")]
        .as_str()
        .unwrap()
        .parse::<i32>()
        .unwrap();
    let start_token = active[&format!("{actor}_start_token")]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(process_start_token(pid).unwrap(), start_token);
    let pidfd = pidfd_open(pid).unwrap();
    validate_pidfd_identity(pidfd.as_raw_fd(), pid, &start_token).unwrap();
    ProcessIdentity {
        pid,
        pidfd,
        start_token,
    }
}

#[cfg(target_os = "linux")]
fn recover_retain_abort(
    repository: &TestRepository,
    roots: &relay::workspace::authority::AuthorityRoots,
    manifest_file: &Path,
    repository_id: &str,
    session_id: &str,
    coordinator_capability_file: &Path,
    label: &str,
) -> relay::workspace::schema::JcsValue {
    let manifest = parse_jcs(&fs::read(manifest_file).unwrap(), true)
        .unwrap()
        .object()
        .unwrap();
    let state = WorkspaceState::parse(manifest["state"].as_str().unwrap()).unwrap();
    let expected_head = git_stdout(
        Path::new(manifest["worktree_root"].as_str().unwrap()),
        ["rev-parse", "HEAD"],
    );
    let request = RecoverRequestV1 {
        request_id: request_id(),
        repository_path: repository.root.to_string_lossy().into_owned(),
        repository_id: repository_id.to_owned(),
        session_id: session_id.to_owned(),
        expected_state: state,
        expected_journal_head_sha256: manifest["journal_head_sha256"].as_str().unwrap().to_owned(),
        expected_head,
        action: "retain_abort".to_owned(),
        created_at: "2026-07-22T13:02:56.789Z".to_owned(),
    };
    let request_file = repository.home.join(format!("{label}-recover-v1.json"));
    let request_sha256 = write_closed_record(&request_file, &request);
    recover_workspace_with_roots(
        roots,
        &request_file,
        &request_sha256,
        coordinator_capability_file,
    )
    .unwrap_or_else(|error| panic!("{label}: {error}"))
}

#[cfg(target_os = "linux")]
fn assert_closed_recovery(value: relay::workspace::schema::JcsValue) {
    let receipt = value.object().unwrap();
    assert_eq!(receipt["outcome"].as_str().unwrap(), "closed");
    assert_eq!(receipt["capabilities_revoked"], JcsValue::Bool(true));
    assert_eq!(receipt["lease_released"], JcsValue::Bool(true));
    assert_eq!(receipt["worktree_removed"], JcsValue::Bool(true));
}

#[test]
fn crashed_writer_recovers_only_after_empty_proof() {
    #[cfg(not(target_os = "linux"))]
    {
        assert_eq!(
            admit_macos_writable_custody_for_test().unwrap_err(),
            MACOS_STOP_REASON
        );
    }
    #[cfg(target_os = "linux")]
    {
        let repository = TestRepository::init("crashed-writer-recovery");
        let roots = isolated_authority_roots(&repository, "crashed-writer-recovery");
        let writer = start_test_workspace(
            &repository,
            &roots,
            "single-custodian-loss",
            vec![PathClaimRequestV1 {
                path: "base.txt".to_owned(),
                path_type: "file".to_owned(),
                mode: "exclusive".to_owned(),
            }],
            None,
        );
        let active_path = writer
            .manifest_file
            .with_file_name("custody-active-v1.json");
        let fault_path = writer.manifest_file.with_file_name("custody-fault-v1.json");
        let empty_path = writer.manifest_file.with_file_name("custody-empty-v1.json");
        let broker_close = writer.manifest_file.with_file_name("broker-close-v1.json");
        let capability_record = writer
            .manifest_file
            .with_file_name("worker-capability-record-v1.json");
        let active = parse_jcs(&fs::read(&active_path).unwrap(), true)
            .unwrap()
            .object()
            .unwrap();
        let cgroup = Path::new("/sys/fs/cgroup").join(
            active["cgroup_membership"]
                .as_str()
                .unwrap()
                .trim_start_matches('/'),
        );
        let supervisor = recorded_custodian(&active_path, "supervisor");
        let guardian_pid = active["guardian_pid"]
            .as_str()
            .unwrap()
            .parse::<i32>()
            .unwrap();
        let guardian_token = active["guardian_start_token"].as_str().unwrap().to_owned();
        let lease_path = writer.lease.path().to_owned();
        let manifest_file = writer.manifest_file.clone();
        let repository_id = writer.result.repository_id.clone();
        let session_id = writer.result.session_id.clone();
        let coordinator_capability = PathBuf::from(&writer.result.coordinator_capability_file);
        let worker_capability = writer.worker_capability_file.clone();
        let resource = temp_dir_resource(&writer);

        signal_pidfd(&supervisor, libc::SIGKILL).unwrap();
        wait_until(
            "single-custodian authenticated fault",
            Duration::from_secs(10),
            || fault_path.exists() && empty_path.exists(),
        );
        assert_eq!(process_start_token(guardian_pid).unwrap(), guardian_token);
        assert!(
            fs::read_to_string(cgroup.join("cgroup.events"))
                .unwrap()
                .lines()
                .any(|line| line == "populated 0")
        );
        wait_until(
            "crash capability revocation",
            Duration::from_secs(10),
            || {
                read_jcs_file::<relay::workspace::schema::CapabilityRecordV1>(
                    &capability_record,
                    None,
                )
                .is_ok_and(|record| record.revoked_at.is_some())
                    && broker_close.exists()
            },
        );
        assert!(!worker_capability.exists());
        drop(writer.lease);
        assert!(
            WorkspaceLease::acquire(&lease_path).is_err(),
            "surviving guardian released the lifetime lease before explicit recovery"
        );
        assert_eq!(
            parse_jcs(&fs::read(&manifest_file).unwrap(), true)
                .unwrap()
                .object()
                .unwrap()["state"]
                .as_str()
                .unwrap(),
            "Running"
        );

        let recovered = recover_retain_abort(
            &repository,
            &roots,
            &manifest_file,
            &repository_id,
            &session_id,
            &coordinator_capability,
            "single-custodian-loss",
        );
        assert_closed_recovery(recovered);
        assert!(!cgroup.exists());
        assert!(!resource.exists());
        let independent = WorkspaceLease::acquire(&lease_path)
            .expect("explicit recovery releases the retained lifetime lease");
        drop(independent);

        let dual = start_test_workspace(
            &repository,
            &roots,
            "dual-custodian-loss",
            vec![PathClaimRequestV1 {
                path: "base.txt".to_owned(),
                path_type: "file".to_owned(),
                mode: "exclusive".to_owned(),
            }],
            Some(&coordinator_capability),
        );
        let dual_active = dual.manifest_file.with_file_name("custody-active-v1.json");
        let guardian = recorded_custodian(&dual_active, "guardian");
        let supervisor = recorded_custodian(&dual_active, "supervisor");
        let active = parse_jcs(&fs::read(&dual_active).unwrap(), true)
            .unwrap()
            .object()
            .unwrap();
        let dual_cgroup = Path::new("/sys/fs/cgroup").join(
            active["cgroup_membership"]
                .as_str()
                .unwrap()
                .trim_start_matches('/'),
        );
        let dual_lease_path = dual.lease.path().to_owned();
        let dual_manifest = dual.manifest_file.clone();
        let dual_repository_id = dual.result.repository_id.clone();
        let dual_session_id = dual.result.session_id.clone();
        let dual_resource = temp_dir_resource(&dual);
        signal_pidfd(&guardian, libc::SIGKILL).unwrap();
        signal_pidfd(&supervisor, libc::SIGKILL).unwrap();
        wait_until(
            "dual-custodian worker EMPTY",
            Duration::from_secs(10),
            || {
                fs::read_to_string(dual_cgroup.join("cgroup.events"))
                    .is_ok_and(|events| events.lines().any(|line| line == "populated 0"))
            },
        );
        drop(dual.lease);
        assert!(
            WorkspaceLease::acquire(&dual_lease_path).is_err(),
            "dual custodian loss released the broker lifetime reference implicitly"
        );
        let recovered = recover_retain_abort(
            &repository,
            &roots,
            &dual_manifest,
            &dual_repository_id,
            &dual_session_id,
            &coordinator_capability,
            "dual-custodian-loss",
        );
        assert_closed_recovery(recovered);
        assert!(!dual_cgroup.exists());
        assert!(!dual_resource.exists());
        let independent = WorkspaceLease::acquire(&dual_lease_path)
            .expect("explicit dual-crash recovery releases the lifetime lease");
        drop(independent);
    }
}

#[test]
fn cgroup_reconcile_removes_empty_orphan_and_refuses_nonempty_or_foreign_identity() {
    #[cfg(target_os = "linux")]
    {
        if std::env::var_os("SESSION_RELAY_TEST_CGROUP_ROOT").is_none() {
            return;
        }

        let empty_session = request_id();
        let empty = DelegatedCgroup::create(&empty_session).unwrap();
        let empty_path = empty.path().to_owned();
        drop(empty);
        reconcile_empty_delegated_cgroup(&empty_session).unwrap();
        assert!(!empty_path.exists(), "empty orphan cgroup was retained");

        let populated_session = request_id();
        let populated = DelegatedCgroup::create(&populated_session).unwrap();
        let populated_path = populated.path().to_owned();
        let child = populated_path.join(request_id());
        fs::create_dir(&child).unwrap();
        drop(populated);
        let error = reconcile_empty_delegated_cgroup(&populated_session).unwrap_err();
        assert!(
            error.contains("child cgroups") || error.contains("nonempty"),
            "wrong nonempty cgroup refusal: {error}"
        );
        fs::remove_dir(&child).unwrap();
        reconcile_empty_delegated_cgroup(&populated_session).unwrap();

        let foreign = reconcile_empty_delegated_cgroup("../foreign").unwrap_err();
        assert!(
            foreign.contains("lowercase UUIDv4"),
            "wrong foreign cgroup identity refusal: {foreign}"
        );
    }
}

#[test]
fn linux_cgroup_pidfd_guardian_kills_hostile_descendants() {
    #[cfg(not(target_os = "linux"))]
    {
        assert_eq!(
            admit_macos_writable_custody_for_test().unwrap_err(),
            MACOS_STOP_REASON,
        );
        return;
    }
    #[cfg(target_os = "linux")]
    {
        if hostile_child_if_requested() {
            return;
        }

        let repository = TestRepository::init("linux-custody");
        let roots = isolated_authority_roots(&repository, "linux-custody");
        let managed = start_test_workspace(
            &repository,
            &roots,
            "native-runtime",
            vec![PathClaimRequestV1 {
                path: "base.txt".to_owned(),
                path_type: "file".to_owned(),
                mode: "exclusive".to_owned(),
            }],
            None,
        );
        assert_eq!(
            manifest_fields(&managed).0,
            relay::workspace::schema::WorkspaceState::Running
        );
        let active_path = managed
            .manifest_file
            .with_file_name("custody-active-v1.json");
        let active = parse_jcs(&fs::read(&active_path).unwrap(), true).unwrap();
        let active = active.object().unwrap();
        assert_eq!(active["schema"].as_str().unwrap(), "CustodyActiveV1");
        assert_eq!(
            active["session_id"].as_str().unwrap(),
            managed.result.session_id
        );
        for actor in ["guardian", "supervisor"] {
            let pid = active[&format!("{actor}_pid")]
                .as_str()
                .unwrap()
                .parse::<i32>()
                .unwrap();
            let expected_token = active[&format!("{actor}_start_token")].as_str().unwrap();
            assert_eq!(process_start_token(pid).unwrap(), expected_token);
            let pidfd = pidfd_open(pid).unwrap();
            validate_pidfd_identity(pidfd.as_raw_fd(), pid, expected_token).unwrap();
        }
        let managed_cgroup = Path::new("/sys/fs/cgroup").join(
            active["cgroup_membership"]
                .as_str()
                .unwrap()
                .trim_start_matches('/'),
        );
        assert_eq!(
            fs::read_to_string(managed_cgroup.join("cgroup.type"))
                .unwrap()
                .trim(),
            "domain"
        );
        assert!(!active["prepared_sha256"].as_str().unwrap().is_empty());
        assert!(!active["activated_sha256"].as_str().unwrap().is_empty());

        let common_git = repository.git_dir();
        let managed_root = PathBuf::from(&managed.result.worktree_root);
        let authoritative_paths = [
            roots.authority.as_path(),
            roots.data.as_path(),
            common_git.as_path(),
            repository.root.as_path(),
            managed_root.as_path(),
        ];
        let mut admitted_mount_id = None;
        for path in authoritative_paths {
            let opened = File::open(path).unwrap();
            let identity = require_ext4_fd(opened.as_raw_fd()).unwrap();
            assert_eq!(identity.filesystem_type, "ext4");
            if let Some(expected) = admitted_mount_id {
                assert_eq!(
                    identity.mount_id, expected,
                    "authoritative paths crossed ext4 mount identities"
                );
            } else {
                admitted_mount_id = Some(identity.mount_id);
            }
        }

        let ready = repository.root.join("hostile-ready");
        let session_id = request_id();
        let cgroup = DelegatedCgroup::create(&session_id)
            .expect("native runner must provide an owned delegated cgroup-v2 root");
        assert_eq!(
            fs::read_to_string(cgroup.path().join("cgroup.type"))
                .unwrap()
                .trim(),
            "domain"
        );
        let executable = std::env::current_exe().unwrap();
        let launch = WorkerLaunch {
            executable,
            arguments: vec![
                "linux_cgroup_pidfd_guardian_kills_hostile_descendants".to_owned(),
                "--exact".to_owned(),
                "--nocapture".to_owned(),
            ],
            environment: BTreeMap::from([(
                "SESSION_RELAY_HOSTILE_READY".to_owned(),
                ready.to_string_lossy().into_owned(),
            )]),
            cwd: repository.root.clone(),
            resource_fds: Vec::new(),
            sandbox: LandlockPolicy {
                workspace: repository.root.clone(),
                readable: Vec::new(),
                executable_runtime: Vec::new(),
                pinned_readable: Vec::new(),
                writable_resources: Vec::new(),
            },
        };
        let prepared = cgroup
            .launch_worker(&launch)
            .expect("prepare confined worker");
        assert!(prepared.prepared_evidence.sandbox_prepared);
        assert_eq!(
            prepared.prepared_evidence.cgroup_membership,
            cgroup.membership()
        );
        let expected_pid = prepared.identity.pid;
        let verified = prepared
            .verify_activation()
            .expect("verify confined worker activation");
        let (identity, activated) = verified
            .release_after_ack(|evidence| {
                assert_eq!(evidence.pid, expected_pid);
                Ok(())
            })
            .expect("release confined worker after activation ACK");
        assert_eq!(activated.pid, identity.pid);
        assert_eq!(
            process_start_token(identity.pid).unwrap(),
            identity.start_token
        );
        wait_until("hostile grandchild", Duration::from_secs(5), || {
            ready.exists()
        });

        let pids = fs::read_to_string(cgroup.path().join("cgroup.procs")).unwrap();
        let pids: Vec<i32> = pids.lines().map(|line| line.parse().unwrap()).collect();
        assert!(
            pids.len() >= 3,
            "fork/setsid descendants escaped the delegated leaf"
        );
        for pid in &pids {
            assert_eq!(
                fs::read_to_string(format!("/proc/{pid}/cgroup"))
                    .unwrap()
                    .lines()
                    .find(|line| line.starts_with("0::"))
                    .unwrap()
                    .trim_start_matches("0::"),
                cgroup.membership()
            );
            let fd_dir = format!("/proc/{pid}/fd");
            for entry in fs::read_dir(fd_dir).unwrap() {
                let target = fs::read_link(entry.unwrap().path()).unwrap_or_default();
                let target = target.to_string_lossy();
                assert!(
                    !target.contains("workspace-authority")
                        && !target.contains("cgroup.events")
                        && !target.contains("cgroup.procs")
                        && !target.contains("cgroup.kill")
                        && !target.contains("broker-v1.sock")
                        && !target.contains("runtime-control-key"),
                    "worker tree inherited custody/authority FD: {target}",
                );
            }
        }

        let empty = cgroup
            .kill_and_wait_empty(&identity)
            .expect("cgroup.kill and recursive populated=0");
        assert!(!empty.populated);
        assert_eq!(empty.cgroup_path, cgroup.membership());
        assert!(
            fs::read_to_string(cgroup.path().join("cgroup.events"))
                .unwrap()
                .lines()
                .any(|line| line == "populated 0")
        );

        let lease_path = repository.home.join("last-close.lease");
        let lease = WorkspaceLease::acquire(&lease_path).unwrap();
        assert!(
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(&lease_path)
                .and_then(|probe| probe_closed_lease(probe.as_raw_fd())
                    .map(|_| ())
                    .map_err(std::io::Error::other))
                .is_err(),
            "independent lease probe succeeded before the last close"
        );
        drop(lease);
        let probe = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lease_path)
            .unwrap();
        let proof =
            probe_closed_lease(probe.as_raw_fd()).expect("independent post-close lease probe");
        assert_ne!(proof.dev, 0);
        assert_ne!(proof.ino, 0);
        cgroup.remove().expect("remove proven-empty delegated leaf");

        let managed_cgroup_copy = managed_cgroup.clone();
        let cleanup = abort_started_workspace(&repository, &roots, managed, "native test cleanup");
        assert!(cleanup.capabilities_revoked);
        assert!(cleanup.lease_released);
        assert!(!managed_cgroup_copy.exists());
    }
}

#[test]
fn macos_process_group_recursive_guardian_kills_hostile_descendants() {
    assert_eq!(MACOS_INADMISSIBLE_BACKEND, "macos_pgroup_libproc");
    let error = admit_macos_writable_custody_for_test().unwrap_err();
    assert_eq!(error, MACOS_STOP_REASON);
    assert_eq!(
        error,
        "process groups are escapable, kqueue is PID observation rather than durable containment, and no documented public primitive provides crash-durable descendant membership plus atomic kill/empty proof",
    );
}
