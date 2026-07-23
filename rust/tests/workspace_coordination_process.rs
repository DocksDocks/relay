pub mod support;

use relay::fanout::{self, FanoutMode, FanoutState, FanoutStore};
use relay::workspace::authority::{
    AuthorityRootProvider, AuthorityRoots, SystemAuthorityRootProvider, WorkspaceAuthority,
};
use relay::workspace::capability;
use relay::workspace::git::{OpenedRepository, actual_private_git_dir};
use relay::workspace::schema::{
    JcsValue, PathClaimRequestV1, WorkspaceState, parse_jcs, validate_non_overlapping_claims,
};
use relay::workspace::{StartedWorkspace, set_integration_fault_for_test};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Barrier};
use std::thread;
use support::fanout::{activate_worker, force_terminal_releasable, seed_entry};
use support::workspace::{
    TestRepository, abort_started_workspace, commit_workspace, finish_prepared_workspace,
    finish_started_workspace, git_ok, git_output, git_stdout, handback_started_workspace,
    handback_started_workspace_output, integrate_prepared_workspace, integrate_started_workspace,
    integrate_started_workspace_result, isolated_authority_roots, manifest_fields,
    prepare_finish_request, prepare_integration_request, prepare_test_workspace,
    runtime_artifact_paths, start_prepared_workspace, start_test_workspace, write_closed_record,
};

struct TestDirectoryCleanup(std::path::PathBuf);

impl Drop for TestDirectoryCleanup {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn claim(path: &str, path_type: &str) -> PathClaimRequestV1 {
    PathClaimRequestV1 {
        path: path.to_owned(),
        path_type: path_type.to_owned(),
        mode: "exclusive".to_owned(),
    }
}

fn manifest_commit_fields(
    started: &StartedWorkspace,
) -> (String, String, Vec<(String, String)>, Vec<String>) {
    let value = parse_jcs(&fs::read(&started.manifest_file).unwrap(), true).unwrap();
    let object = value.object().unwrap();
    let produced = match &object["produced_commits"] {
        JcsValue::Array(values) => values
            .iter()
            .map(|value| {
                let commit = value.clone().object().unwrap();
                (
                    commit["oid"].as_str().unwrap().to_owned(),
                    commit["source"].as_str().unwrap().to_owned(),
                )
            })
            .collect(),
        _ => panic!("manifest produced_commits is not an array"),
    };
    let integrated = match &object["integration_commits"] {
        JcsValue::Array(values) => values
            .iter()
            .map(|value| value.as_str().unwrap().to_owned())
            .collect(),
        _ => panic!("manifest integration_commits is not an array"),
    };
    (
        object["applied_wip_commit"].as_str().unwrap().to_owned(),
        object["worker_base_commit"].as_str().unwrap().to_owned(),
        produced,
        integrated,
    )
}

fn manifest_fields_from_path(path: &Path) -> WorkspaceState {
    let value = parse_jcs(&fs::read(path).unwrap(), true).unwrap();
    WorkspaceState::parse(value.object().unwrap()["state"].as_str().unwrap()).unwrap()
}

fn authority(
    repo: &TestRepository,
) -> (
    WorkspaceAuthority,
    relay::workspace::schema::RepositoryIdentityV1,
) {
    let authority_root = repo.home.join("authority");
    let data_root = repo.home.join("data");
    fs::create_dir(&authority_root).unwrap();
    fs::create_dir(&data_root).unwrap();
    fs::set_permissions(&authority_root, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(&data_root, fs::Permissions::from_mode(0o700)).unwrap();
    let roots = AuthorityRoots {
        authority: authority_root,
        data: data_root,
        euid: unsafe { libc::geteuid() },
    };
    let authority = WorkspaceAuthority::new(roots).unwrap();
    let identity = OpenedRepository::open(&repo.root).unwrap().identity;
    (authority, identity)
}

#[test]
fn overlapping_path_claims_are_atomic_and_refused() {
    fn race_case(
        label: &str,
        left_claims: Vec<PathClaimRequestV1>,
        right_claims: Vec<PathClaimRequestV1>,
        default_vs_owned: bool,
    ) {
        let mut repo = TestRepository::init(&format!("claim-race-{label}"));
        fs::create_dir_all(repo.root.join("src/lib")).unwrap();
        fs::write(repo.root.join("src/lib.rs"), b"lib\n").unwrap();
        fs::create_dir_all(repo.root.join("Src")).unwrap();
        fs::write(repo.root.join("src/lib/deep.txt"), b"deep\n").unwrap();
        fs::write(repo.root.join("Src/lib.rs"), b"case-variant lib\n").unwrap();
        fs::write(
            repo.root.join("Cargo.toml"),
            b"[package]\nname = \"claim-race\"\n",
        )
        .unwrap();
        git_ok(&repo.root, ["add", "--", "."]);
        git_ok(
            &repo.root,
            ["commit", "--quiet", "-m", "seed claim targets"],
        );
        repo.base_commit = git_stdout(&repo.root, ["rev-parse", "HEAD"]);

        let roots = isolated_authority_roots(&repo, label);
        let authority = WorkspaceAuthority::new(roots.clone()).unwrap();
        let identity = OpenedRepository::open(&repo.root).unwrap().identity;
        let bootstrap = authority
            .bootstrap_coordinator(&identity, "2026-07-22T12:34:56.789Z")
            .unwrap();
        let left = prepare_test_workspace(&repo, &roots, "left", "commit", left_claims);
        let mut right = prepare_test_workspace(&repo, &roots, "right", "commit", right_claims);
        if default_vs_owned {
            right.request.coordinator_owned_overrides = vec![JcsValue::Object(BTreeMap::from([
                ("path".into(), JcsValue::String("Cargo.toml".into())),
                (
                    "reason".into(),
                    JcsValue::String("exercise default-vs-owned admission".into()),
                ),
            ]))];
            right.request_sha256 = write_closed_record(&right.request_file, &right.request);
        }
        let request_ids = [
            left.request.request_id.clone(),
            right.request.request_id.clone(),
        ];
        let before = repo.snapshot();
        let barrier = Arc::new(Barrier::new(3));
        let handles = [left, right]
            .into_iter()
            .map(|prepared| {
                let roots = roots.clone();
                let capability = bootstrap.capability_file.clone();
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    start_prepared_workspace(&roots, &prepared, Some(&capability))
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        let mut results = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();
        let errors = results
            .iter()
            .filter_map(|result| result.as_ref().err())
            .collect::<Vec<_>>();
        assert_eq!(
            results.iter().filter(|result| result.is_ok()).count(),
            1,
            "{label}: overlapping admission did not have exactly one complete winner: {errors:?}"
        );
        let loser = results
            .iter()
            .find_map(|result| result.as_ref().err())
            .unwrap();
        assert!(
            loser.contains("overlap") || loser.contains("coordinator-owned"),
            "{label}: wrong atomic loser refusal: {loser}"
        );
        let winner_index = results.iter().position(Result::is_ok).unwrap();
        let loser_index = 1 - winner_index;
        let started = results.swap_remove(winner_index).unwrap();
        assert_eq!(
            manifest_fields(&started).0,
            WorkspaceState::Running,
            "{label}: winner did not durably reach Running"
        );
        let sessions = authority
            .repository_dir(&identity.repository_id)
            .unwrap()
            .join("sessions");
        assert_eq!(
            fs::read_dir(&sessions).unwrap().count(),
            1,
            "{label}: loser left partial session authority"
        );
        assert!(
            !sessions.join(&request_ids[loser_index]).exists(),
            "{label}: loser published a session directory"
        );
        let losing_ref = format!(
            "refs/heads/docks/{}/{}",
            request_ids[loser_index],
            if loser_index == 0 { "left" } else { "right" }
        );
        assert!(
            !git_output(&repo.root, ["show-ref", "--verify", "--quiet", &losing_ref])
                .status
                .success(),
            "{label}: loser published a shared branch ref"
        );
        let after = repo.snapshot();
        assert_eq!(after.head, before.head, "{label}: source HEAD changed");
        assert_eq!(after.status, before.status, "{label}: source bytes changed");
        assert_eq!(after.index, before.index, "{label}: source index changed");
        assert_eq!(after.tree, before.tree, "{label}: source tree changed");
        assert_eq!(
            started.resources.allocations.len(),
            1,
            "{label}: winner resource set is incomplete"
        );
        let cleanup = abort_started_workspace(&repo, &roots, started, "claim race cleanup");
        assert!(cleanup.lease_released, "{label}: winner lease leaked");
        assert_eq!(
            cleanup.resource_receipts.len(),
            1,
            "{label}: resource leaked"
        );
    }

    assert!(
        validate_non_overlapping_claims(&[
            claim("src/lib.rs", "file"),
            claim("src/lib.rs", "file")
        ])
        .is_err()
    );
    assert!(
        validate_non_overlapping_claims(&[claim("src", "directory"), claim("src/lib.rs", "file")])
            .is_err()
    );
    assert!(
        validate_non_overlapping_claims(&[
            claim("Src/lib.rs", "file"),
            claim("src/lib.rs", "file")
        ])
        .is_err()
    );
    assert!(
        validate_non_overlapping_claims(&[
            claim("src/lib", "directory"),
            claim("src/library", "directory")
        ])
        .is_ok()
    );

    race_case(
        "file",
        vec![claim("base.txt", "file")],
        vec![claim("base.txt", "file")],
        false,
    );
    race_case(
        "directory",
        vec![claim("src", "directory")],
        vec![claim("src/lib.rs", "file")],
        false,
    );
    race_case(
        "prefix",
        vec![claim("src/lib", "directory")],
        vec![claim("src/lib/deep.txt", "file")],
        false,
    );
    race_case(
        "case",
        vec![claim("src/lib.rs", "file")],
        vec![claim("Src/lib.rs", "file")],
        false,
    );
    race_case(
        "default",
        Vec::new(),
        vec![claim("Cargo.toml", "file")],
        true,
    );
}

#[test]
fn coordinator_bootstrap_worker_scope_and_replay_are_closed() {
    if !cfg!(target_os = "linux") {
        eprintln!("SKIP native-only: writable workspace custody requires Linux");
        return;
    }
    let mut repo = TestRepository::init("bootstrap-race");
    fs::write(repo.root.join("left.txt"), b"left\n").unwrap();
    fs::write(repo.root.join("right.txt"), b"right\n").unwrap();
    git_ok(&repo.root, ["add", "--", "left.txt", "right.txt"]);
    git_ok(
        &repo.root,
        ["commit", "--quiet", "-m", "seed bootstrap claims"],
    );
    repo.base_commit = git_stdout(&repo.root, ["rev-parse", "HEAD"]);
    let roots = isolated_authority_roots(&repo, "bootstrap-race");
    let authority = WorkspaceAuthority::new(roots.clone()).unwrap();
    let identity = OpenedRepository::open(&repo.root).unwrap().identity;
    let left = prepare_test_workspace(
        &repo,
        &roots,
        "left",
        "commit",
        vec![claim("left.txt", "file")],
    );
    let right = prepare_test_workspace(
        &repo,
        &roots,
        "right",
        "commit",
        vec![claim("right.txt", "file")],
    );
    let request_ids = [
        left.request.request_id.clone(),
        right.request.request_id.clone(),
    ];
    let before = repo.snapshot();
    let barrier = Arc::new(Barrier::new(3));
    let handles = [left, right]
        .into_iter()
        .map(|prepared| {
            let roots = roots.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                start_prepared_workspace(&roots, &prepared, None)
            })
        })
        .collect::<Vec<_>>();
    barrier.wait();
    let mut results = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect::<Vec<_>>();
    let errors = results
        .iter()
        .filter_map(|result| result.as_ref().err())
        .collect::<Vec<_>>();
    assert_eq!(
        results.iter().filter(|result| result.is_ok()).count(),
        1,
        "first-start race did not have exactly one complete winner: {errors:?}"
    );
    let loser = results
        .iter()
        .find_map(|result| result.as_ref().err())
        .unwrap();
    let current_path = authority
        .capability_path(&identity.repository_id, 1)
        .unwrap();
    assert!(
        loser.contains(current_path.to_string_lossy().as_ref()),
        "bootstrap loser did not name deterministic generation-1 path: {loser}"
    );
    let winner_index = results.iter().position(Result::is_ok).unwrap();
    let loser_index = 1 - winner_index;
    let started = results.swap_remove(winner_index).unwrap();
    assert_eq!(started.result.bootstrap, "created");
    assert_eq!(started.result.coordinator_generation, "1");
    assert_eq!(
        Path::new(&started.result.coordinator_capability_file),
        current_path
    );
    assert_eq!(manifest_fields(&started).0, WorkspaceState::Running);
    let repository_dir = authority.repository_dir(&identity.repository_id).unwrap();
    assert_eq!(
        fs::read_dir(repository_dir.join("sessions"))
            .unwrap()
            .count(),
        1
    );
    assert!(
        !repository_dir
            .join("sessions")
            .join(&request_ids[loser_index])
            .exists(),
        "bootstrap loser left partial session state"
    );
    let after = repo.snapshot();
    assert_eq!(after.head, before.head);
    assert_eq!(after.status, before.status);
    assert_eq!(after.index, before.index);
    assert_eq!(after.tree, before.tree);

    let exact_bytes = fs::read(&current_path).unwrap();
    let (record, current) = authority
        .authenticate(&identity.repository_id, &current_path, "integrate")
        .expect("current exact capability authenticates");
    assert_eq!(record.current_generation, "1");
    assert!(
        authority
            .authenticate(&"0".repeat(64), &current_path, "integrate")
            .is_err(),
        "repository UUID/path possession became authority"
    );
    let copied = repo.home.join("copied-capability.json");
    fs::copy(&current_path, &copied).unwrap();
    fs::set_permissions(&copied, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(
        authority
            .authenticate(&identity.repository_id, &copied, "integrate")
            .is_err(),
        "copied current capability authenticated"
    );
    assert!(
        capability::authenticate_coordinator(
            &current,
            &record.coordinator,
            &identity.repository_id,
            1,
            "git_commit",
        )
        .is_err()
    );

    let cleanup = abort_started_workspace(&repo, &roots, started, "bootstrap replay cleanup");
    assert!(cleanup.lease_released);
    let rotated = authority
        .rotate_coordinator(
            &identity.repository_id,
            &current_path,
            "2026-07-22T12:35:56.789Z",
        )
        .expect("rotate current capability");
    let rotated_record = authority.read_repository(&identity.repository_id).unwrap();
    assert!(
        authority
            .authenticate(&identity.repository_id, &current_path, "integrate")
            .is_err(),
        "stale/revoked generation authenticated"
    );
    assert!(
        authority
            .authenticate(
                &identity.repository_id,
                &rotated.capability_file,
                "integrate"
            )
            .is_ok()
    );
    let rotated_bytes = fs::read(&rotated.capability_file).unwrap();
    let mut changed = parse_jcs(&rotated_bytes, true).unwrap().object().unwrap();
    let mut changed_secret = changed["secret_b64url"]
        .as_str()
        .unwrap()
        .as_bytes()
        .to_vec();
    changed_secret[0] = if changed_secret[0] == b'A' {
        b'B'
    } else {
        b'A'
    };
    changed.insert(
        "secret_b64url".into(),
        JcsValue::String(String::from_utf8(changed_secret).unwrap()),
    );
    fs::write(
        &rotated.capability_file,
        format!(
            "{}\n",
            relay::workspace::schema::serialize_jcs(&JcsValue::Object(changed))
        ),
    )
    .unwrap();
    assert!(
        authority
            .authenticate(
                &identity.repository_id,
                &rotated.capability_file,
                "integrate"
            )
            .is_err(),
        "changed exact-path replacement authenticated"
    );
    fs::write(&rotated.capability_file, &rotated_bytes).unwrap();
    fs::set_permissions(&rotated.capability_file, fs::Permissions::from_mode(0o600)).unwrap();
    let identical_one = authority
        .authenticate(
            &identity.repository_id,
            &rotated.capability_file,
            "integrate",
        )
        .unwrap();
    let identical_two = authority
        .authenticate(
            &identity.repository_id,
            &rotated.capability_file,
            "integrate",
        )
        .unwrap();
    assert_eq!(
        identical_one, identical_two,
        "identical replay was not stable"
    );
    assert_eq!(
        authority.read_repository(&identity.repository_id).unwrap(),
        rotated_record,
        "changed/identical replay mutated repository authority"
    );
    assert_eq!(rotated_record.current_generation, "2");
    assert_eq!(fs::read(&current_path).unwrap(), exact_bytes);
}

#[test]
fn recovery_matrix_has_no_unproven_progress() {
    let states = [
        WorkspaceState::Reserved,
        WorkspaceState::Provisioning,
        WorkspaceState::LeaseHeld,
        WorkspaceState::Ready,
        WorkspaceState::Running,
        WorkspaceState::HandbackReady,
        WorkspaceState::IntegrationQueued,
        WorkspaceState::IntegrationBlocked,
        WorkspaceState::Rejected,
        WorkspaceState::AbortedRetained,
    ];
    for state in states {
        assert!(
            !state.may_transition_to(WorkspaceState::Closed),
            "{state:?} bypassed release proof"
        );
    }
    assert!(
        !WorkspaceState::IntegrationBlocked.may_transition_to(WorkspaceState::IntegrationQueued)
    );
    assert!(WorkspaceState::Releasing.may_transition_to(WorkspaceState::Closed));
    if cfg!(target_os = "linux") && std::env::var_os("SESSION_RELAY_TEST_CGROUP_ROOT").is_some() {
        for point in [
            "after_pre_index",
            "after_progress",
            "after_queue",
            "after_git_step",
            "after_step_progress",
            "after_receipt",
        ] {
            let repo = TestRepository::init(&format!("integration-recovery-{point}"));
            let roots = isolated_authority_roots(&repo, &format!("integration-recovery-{point}"));
            let task_slug = point.replace('_', "-");
            let started = start_test_workspace(
                &repo,
                &roots,
                &task_slug,
                vec![claim("base.txt", "file")],
                None,
            );
            commit_workspace(
                &started,
                "base.txt",
                format!("{point}\n").as_bytes(),
                &format!("worker {point}"),
            );
            handback_started_workspace(&repo, &started);
            let prepared = prepare_integration_request(&repo, &started);
            let source_before = repo.snapshot();
            set_integration_fault_for_test(&started.result.session_id, point).unwrap();
            let injected = integrate_prepared_workspace(&roots, &prepared).unwrap_err();
            assert!(
                injected.contains("injected integration fault"),
                "{point}: wrong integration fault: {injected}"
            );
            let receipt = integrate_prepared_workspace(&roots, &prepared).unwrap();
            assert_eq!(receipt.request_id, prepared.request.request_id);
            assert_eq!(receipt.outcome, "integrated");
            assert_eq!(
                receipt.integration_commits.len(),
                receipt.worker_commits.len(),
                "{point}: retry lost or duplicated an integration step"
            );
            assert_eq!(
                git_stdout(&repo.root, ["rev-parse", "HEAD"]),
                receipt.post_integration_head
            );
            assert_eq!(git_stdout(&repo.root, ["status", "--porcelain"]), "");
            assert_eq!(
                fs::read(repo.root.join("base.txt")).unwrap(),
                format!("{point}\n").as_bytes()
            );
            assert_eq!(
                source_before.head, receipt.pre_integration_head,
                "{point}: recovery changed the bound pre-integration head"
            );
            let replay = integrate_prepared_workspace(&roots, &prepared).unwrap();
            assert_eq!(replay, receipt, "{point}: identical receipt replay changed");
            let cleanup = finish_started_workspace(&repo, &roots, started);
            assert!(cleanup.lease_released);
        }
    }

    let repo = TestRepository::init("missing-secret-recovery");
    let (authority, identity) = authority(&repo);
    let bootstrap = authority
        .bootstrap_coordinator(&identity, "2026-07-22T12:34:56.789Z")
        .unwrap();
    fs::remove_file(&bootstrap.capability_file).unwrap();
    let repository_before = authority.read_repository(&identity.repository_id).unwrap();
    assert!(
        authority
            .rotate_coordinator(
                &identity.repository_id,
                &bootstrap.capability_file,
                "2026-07-22T12:35:56.789Z",
            )
            .is_err()
    );
    assert_eq!(
        authority.read_repository(&identity.repository_id).unwrap(),
        repository_before
    );
    assert!(
        !authority
            .capability_path(&identity.repository_id, 2)
            .unwrap()
            .exists()
    );
}

#[test]
fn unexpected_branch_switch_is_refused() {
    if !cfg!(target_os = "linux") {
        eprintln!("SKIP native-only: writable workspace custody requires Linux");
        return;
    }
    fn git_state(worktree: &Path) -> (Vec<u8>, Vec<u8>, String, String, Vec<u8>) {
        let private = actual_private_git_dir(worktree).unwrap();
        (
            git_output(
                worktree,
                ["status", "--porcelain=v2", "-z", "--untracked-files=all"],
            )
            .stdout,
            fs::read(private.join("index")).unwrap(),
            git_stdout(worktree, ["symbolic-ref", "-q", "HEAD"]),
            git_stdout(worktree, ["rev-parse", "HEAD"]),
            git_output(worktree, ["count-objects", "-v"]).stdout,
        )
    }

    let repo = TestRepository::init("coordinator-branch-drift");
    let roots = isolated_authority_roots(&repo, "coordinator-branch-drift");
    let started = start_test_workspace(
        &repo,
        &roots,
        "drift",
        vec![claim("base.txt", "file")],
        None,
    );
    let worktree = Path::new(&started.result.worktree_root);
    let expected_branch = started.result.branch_ref.clone();
    git_ok(worktree, ["switch", "-c", "unexpected"]);
    fs::write(worktree.join("base.txt"), b"branch drift must be refused\n").unwrap();
    let source_before = repo.snapshot();
    let worktree_before = git_state(worktree);
    let manifest_before = fs::read(&started.manifest_file).unwrap();
    let private = actual_private_git_dir(worktree).unwrap();
    let git_shim = private.join("session-relay/bin/git");
    let broker = Command::new(&git_shim)
        .args(["add", "--", "base.txt"])
        .current_dir(worktree)
        .output()
        .unwrap();
    assert!(
        !broker.status.success(),
        "broker accepted unexpected branch"
    );
    assert!(
        String::from_utf8_lossy(&broker.stderr).contains("branch"),
        "wrong broker branch refusal: {}",
        String::from_utf8_lossy(&broker.stderr)
    );
    assert_eq!(repo.snapshot(), source_before, "broker mutated shared Git");
    assert_eq!(
        git_state(worktree),
        worktree_before,
        "broker mutated worker Git"
    );
    assert_eq!(
        fs::read(&started.manifest_file).unwrap(),
        manifest_before,
        "broker advanced workspace state"
    );

    let handback = handback_started_workspace_output(&repo, &started);
    assert!(
        !handback.status.success(),
        "handback accepted unexpected symbolic branch"
    );
    assert!(
        String::from_utf8_lossy(&handback.stderr).contains("branch"),
        "wrong handback branch refusal: {}",
        String::from_utf8_lossy(&handback.stderr)
    );
    assert_eq!(
        repo.snapshot(),
        source_before,
        "handback mutated shared Git"
    );
    assert_eq!(
        git_state(worktree),
        worktree_before,
        "handback mutated worker objects/index/ref/state"
    );
    assert_eq!(
        fs::read(&started.manifest_file).unwrap(),
        manifest_before,
        "handback advanced workspace state"
    );

    fs::remove_file(worktree.join("base.txt")).unwrap();
    git_ok(worktree, ["restore", "--source=HEAD", "--", "base.txt"]);
    git_ok(
        worktree,
        [
            "switch",
            expected_branch
                .strip_prefix("refs/heads/")
                .expect("workspace branch is a full branch ref"),
        ],
    );
    let cleanup = abort_started_workspace(&repo, &roots, started, "branch drift cleanup");
    assert!(cleanup.lease_released);
}

#[test]
fn applied_wip_is_first_produced_and_integrated_commit() {
    if !cfg!(target_os = "linux") {
        eprintln!("SKIP native-only: writable workspace custody requires Linux");
        return;
    }
    for (label, mode, dirty) in [
        ("commit", "commit", true),
        ("artifact", "artifact", true),
        ("clean", "commit", false),
    ] {
        let repo = TestRepository::init(&format!("applied-first-{label}"));
        if dirty {
            fs::write(repo.root.join("base.txt"), format!("{label} WIP\n")).unwrap();
            if mode == "artifact" {
                fs::write(repo.root.join("untracked.bin"), [0, 1, 2, 0xff]).unwrap();
            }
        }
        let roots = isolated_authority_roots(&repo, &format!("applied-first-{label}"));
        let before_preserve = repo.snapshot();
        let prepared =
            prepare_test_workspace(&repo, &roots, label, mode, vec![claim("base.txt", "file")]);
        let after_preserve = repo.snapshot();
        assert_eq!(
            after_preserve.head, before_preserve.head,
            "{label}: preserve changed HEAD"
        );
        assert_eq!(
            after_preserve.status, before_preserve.status,
            "{label}: preserve changed source bytes"
        );
        assert_eq!(
            after_preserve.index, before_preserve.index,
            "{label}: preserve changed source index"
        );
        assert_eq!(
            after_preserve.tree, before_preserve.tree,
            "{label}: preserve changed tree"
        );
        git_ok(&repo.root, ["reset", "--hard", "--quiet", "HEAD"]);
        git_ok(&repo.root, ["clean", "-fd", "--quiet"]);
        let started = start_prepared_workspace(&roots, &prepared, None).unwrap();
        let worktree = Path::new(&started.result.worktree_root);
        let (applied, worker_base, produced_before_worker, integrated_before_worker) =
            manifest_commit_fields(&started);
        assert_eq!(
            worker_base, applied,
            "{label}: worker base is not applied WIP"
        );
        assert_eq!(
            produced_before_worker,
            vec![(applied.clone(), "applied_wip".into())],
            "{label}: exactly one applied WIP was not durable before worker execution"
        );
        assert!(
            integrated_before_worker.is_empty(),
            "{label}: integration was published before worker execution"
        );
        assert_eq!(
            git_stdout(
                worktree,
                [
                    "rev-list",
                    "--reverse",
                    &format!("{}..HEAD", repo.base_commit)
                ]
            )
            .lines()
            .collect::<Vec<_>>(),
            vec![applied.as_str()],
            "{label}: worker began with more than one produced commit"
        );
        assert_eq!(
            git_stdout(worktree, ["rev-parse", &format!("{applied}^")]),
            repo.base_commit,
            "{label}: applied WIP is not based on the preserved base"
        );

        let worker = commit_workspace(
            &started,
            "base.txt",
            format!("{label} worker\n").as_bytes(),
            &format!("{label} worker"),
        );
        handback_started_workspace(&repo, &started);
        let (_, _, produced_after_handback, _) = manifest_commit_fields(&started);
        assert_eq!(
            produced_after_handback,
            vec![
                (applied.clone(), "applied_wip".into()),
                (worker.clone(), "worker".into())
            ],
            "{label}: handback duplicated or reordered the applied WIP"
        );
        let receipt = integrate_started_workspace(&repo, &roots, &started);
        assert_eq!(receipt.worker_commits, vec![applied.clone(), worker]);
        assert_eq!(receipt.integration_commits.len(), 2);
        assert_eq!(
            git_stdout(
                &repo.root,
                ["rev-parse", &format!("{}^", receipt.integration_commits[0])]
            ),
            receipt.pre_integration_head,
            "{label}: integration_commits[0] was not applied first"
        );
        assert_eq!(
            git_stdout(
                &repo.root,
                ["rev-parse", &format!("{}^", receipt.integration_commits[1])]
            ),
            receipt.integration_commits[0],
            "{label}: worker commit was not integrated after applied WIP"
        );
        assert_eq!(
            git_stdout(
                &repo.root,
                ["show", "-s", "--format=%T", &receipt.integration_commits[0]]
            ),
            git_stdout(worktree, ["show", "-s", "--format=%T", &applied]),
            "{label}: integrated applied-WIP tree differs from worker_base"
        );
        let (_, final_base, final_produced, final_integrated) = manifest_commit_fields(&started);
        assert_eq!(final_base, applied);
        assert_eq!(final_produced[0].0, applied);
        assert_eq!(final_integrated, receipt.integration_commits);
        let cleanup = finish_started_workspace(&repo, &roots, started);
        assert!(cleanup.lease_released, "{label}: lease leaked");
    }
}

#[test]
fn workspace_and_legacy_fanout_share_repository_gate() {
    if !cfg!(target_os = "linux") {
        eprintln!("SKIP native-only: writable workspace custody requires Linux");
        return;
    }
    let mut repo = TestRepository::init("repository-gate");
    let legacy_home = repo.home.join("legacy-store");
    fs::create_dir(&legacy_home).unwrap();
    let invoker = "11111111-1111-4111-8111-111111111111";
    let root_session = "22222222-2222-4222-8222-222222222222";
    let child_session = "33333333-3333-4333-8333-333333333333";
    seed_entry(&legacy_home, invoker, &repo.root);
    let fanout = FanoutStore::new(legacy_home.clone());
    let root = fanout::prepare_worktree(&fanout, &repo.root, invoker, FanoutMode::Root).unwrap();
    let root_worker = activate_worker(
        &fanout,
        &root.reservation_id,
        root_session,
        Path::new(&root.worktree),
    );
    seed_entry(&legacy_home, root_session, Path::new(&root.worktree));
    let child = fanout::prepare_worktree(
        &fanout,
        Path::new(&root.worktree),
        root_session,
        FanoutMode::Child,
    )
    .unwrap();
    let child_worker = activate_worker(
        &fanout,
        &child.reservation_id,
        child_session,
        Path::new(&child.worktree),
    );
    fs::write(Path::new(&child.worktree).join("legacy.txt"), b"legacy\n").unwrap();
    git_ok(Path::new(&child.worktree), ["add", "--", "legacy.txt"]);
    git_ok(
        Path::new(&child.worktree),
        ["commit", "--quiet", "-m", "legacy child"],
    );
    let child_handback = fanout::handback(&fanout, child_session, "completed", "ready").unwrap();
    force_terminal_releasable(&legacy_home, &child_worker);
    let child_collected = fanout::collect(&fanout, child_session, root_session).unwrap();
    assert_eq!(child_collected.state, FanoutState::Collected);
    assert_eq!(
        child_handback.handback_head,
        Some(git_stdout(
            Path::new(&root.worktree),
            ["rev-parse", "HEAD^2"]
        ))
    );
    assert_eq!(
        fs::read(Path::new(&root.worktree).join("legacy.txt")).unwrap(),
        b"legacy\n"
    );
    assert!(!Path::new(&child.worktree).exists());

    seed_entry(&legacy_home, invoker, &repo.root);
    let root_handback = fanout::handback(&fanout, root_session, "completed", "ready").unwrap();
    force_terminal_releasable(&legacy_home, &root_worker);
    let root_collected = fanout::collect(&fanout, root_session, invoker).unwrap();
    assert_eq!(root_collected.state, FanoutState::Collected);
    assert_eq!(
        root_handback.handback_head,
        Some(git_stdout(&repo.root, ["rev-parse", "HEAD^2"]))
    );
    assert_eq!(fs::read(repo.root.join("legacy.txt")).unwrap(), b"legacy\n");
    assert!(!Path::new(&root.worktree).exists());
    repo.base_commit = git_stdout(&repo.root, ["rev-parse", "HEAD"]);

    let roots = SystemAuthorityRootProvider.roots().unwrap();
    let authority = WorkspaceAuthority::new(roots.clone()).unwrap();
    let identity = OpenedRepository::open(&repo.root).unwrap().identity;
    let _authority_cleanup =
        TestDirectoryCleanup(authority.repository_dir(&identity.repository_id).unwrap());
    let started = start_test_workspace(
        &repo,
        &roots,
        "managed",
        vec![claim("base.txt", "file")],
        None,
    );
    commit_workspace(&started, "base.txt", b"managed\n", "managed worker");
    handback_started_workspace(&repo, &started);
    integrate_started_workspace(&repo, &roots, &started);
    finish_started_workspace(&repo, &roots, started);
    let marker = repo.git_dir().join("docks/workspace-admission-v1.json");
    assert!(marker.is_file());

    seed_entry(&legacy_home, invoker, &repo.root);
    let before = repo.snapshot();
    let refused =
        fanout::prepare_worktree(&fanout, &repo.root, invoker, FanoutMode::Root).unwrap_err();
    assert!(refused.contains("managed workspace mode"), "{refused}");
    assert_eq!(
        repo.snapshot(),
        before,
        "old-mode refusal mutated repository"
    );
    assert!(
        fs::read_dir(legacy_home.join("worktrees"))
            .map(|entries| entries.count())
            .unwrap_or(0)
            == 0,
        "old-mode refusal provisioned a legacy worktree"
    );
}

#[test]
fn coordinator_integrates_commits_serially() {
    if !cfg!(target_os = "linux") {
        eprintln!("SKIP native-only: writable workspace custody requires Linux");
        return;
    }
    let mut repo = TestRepository::init("ordered-integration");
    fs::write(repo.root.join("first.txt"), b"").unwrap();
    fs::write(repo.root.join("second.txt"), b"").unwrap();
    git_ok(&repo.root, ["add", "--", "first.txt", "second.txt"]);
    git_ok(
        &repo.root,
        ["commit", "--quiet", "-m", "seed claimed files"],
    );
    repo.base_commit = git_stdout(&repo.root, ["rev-parse", "HEAD"]);
    let roots = isolated_authority_roots(&repo, "ordered-integration");
    let first = start_test_workspace(
        &repo,
        &roots,
        "first",
        vec![claim("first.txt", "file")],
        None,
    );
    let second = start_test_workspace(
        &repo,
        &roots,
        "second",
        vec![claim("second.txt", "file")],
        Some(Path::new(&first.result.coordinator_capability_file)),
    );
    let (first_applied, first_base, first_initial, _) = manifest_commit_fields(&first);
    let (second_applied, second_base, second_initial, _) = manifest_commit_fields(&second);
    assert_eq!(first_base, first_applied);
    assert_eq!(second_base, second_applied);
    assert_eq!(
        first_initial,
        vec![(first_applied.clone(), "applied_wip".into())],
        "first workspace did not durably publish applied WIP at index zero"
    );
    assert_eq!(
        second_initial,
        vec![(second_applied.clone(), "applied_wip".into())],
        "second workspace did not durably publish applied WIP at index zero"
    );

    let first_worker_one =
        commit_workspace(&first, "first.txt", b"first one\n", "first worker one");
    let first_worker_two =
        commit_workspace(&first, "first.txt", b"first two\n", "first worker two");
    let second_worker_one =
        commit_workspace(&second, "second.txt", b"second one\n", "second worker one");
    let second_worker_two =
        commit_workspace(&second, "second.txt", b"second two\n", "second worker two");
    handback_started_workspace(&repo, &first);
    handback_started_workspace(&repo, &second);
    let (_, _, first_produced, _) = manifest_commit_fields(&first);
    let (_, _, second_produced, _) = manifest_commit_fields(&second);
    assert_eq!(
        first_produced,
        vec![
            (first_applied.clone(), "applied_wip".into()),
            (first_worker_one.clone(), "worker".into()),
            (first_worker_two.clone(), "worker".into()),
        ]
    );
    assert_eq!(
        second_produced,
        vec![
            (second_applied.clone(), "applied_wip".into()),
            (second_worker_one.clone(), "worker".into()),
            (second_worker_two.clone(), "worker".into()),
        ]
    );

    let first_receipt = integrate_started_workspace(&repo, &roots, &first);
    let second_receipt = integrate_started_workspace(&repo, &roots, &second);
    assert_eq!(
        first_receipt.worker_commits,
        vec![first_applied, first_worker_one, first_worker_two]
    );
    assert_eq!(
        second_receipt.worker_commits,
        vec![second_applied, second_worker_one, second_worker_two]
    );
    assert_eq!(first_receipt.integration_commits.len(), 3);
    assert_eq!(second_receipt.integration_commits.len(), 3);
    assert_eq!(first_receipt.pre_integration_head, repo.base_commit);
    assert_eq!(
        second_receipt.pre_integration_head,
        first_receipt.post_integration_head
    );
    for receipt in [&first_receipt, &second_receipt] {
        let mut parent = receipt.pre_integration_head.as_str();
        for integrated in &receipt.integration_commits {
            assert_eq!(
                git_stdout(&repo.root, ["rev-parse", &format!("{integrated}^")]),
                parent,
                "integration chain was not imported oldest-first"
            );
            parent = integrated;
        }
        assert_eq!(receipt.post_integration_head, parent);
    }
    let (_, _, _, first_integrated) = manifest_commit_fields(&first);
    let (_, _, _, second_integrated) = manifest_commit_fields(&second);
    assert_eq!(first_integrated, first_receipt.integration_commits);
    assert_eq!(second_integrated, second_receipt.integration_commits);
    assert_eq!(
        git_stdout(&repo.root, ["rev-parse", "HEAD"]),
        second_receipt.post_integration_head
    );
    assert_eq!(
        fs::read(repo.root.join("first.txt")).unwrap(),
        b"first two\n"
    );
    assert_eq!(
        fs::read(repo.root.join("second.txt")).unwrap(),
        b"second two\n"
    );

    let first_cleanup = finish_started_workspace(&repo, &roots, first);
    let second_cleanup = finish_started_workspace(&repo, &roots, second);
    assert!(first_cleanup.lease_released);
    assert!(second_cleanup.lease_released);
}

#[test]
fn conflicting_commits_settle_once_needs_user_action() {
    if !cfg!(target_os = "linux") {
        eprintln!("SKIP native-only: writable workspace custody requires Linux");
        return;
    }
    let repo = TestRepository::init("conflict-rollback");
    let roots = isolated_authority_roots(&repo, "conflict-rollback");
    let first = start_test_workspace(
        &repo,
        &roots,
        "first",
        vec![claim("base.txt", "file")],
        None,
    );
    commit_workspace(&first, "base.txt", b"first\n", "first worker");
    handback_started_workspace(&repo, &first);
    let first_receipt = integrate_started_workspace(&repo, &roots, &first);
    assert_eq!(first_receipt.outcome, "integrated");

    let second = start_test_workspace(
        &repo,
        &roots,
        "second",
        vec![claim("base.txt", "file")],
        Some(std::path::Path::new(
            &first.result.coordinator_capability_file,
        )),
    );
    commit_workspace(&second, "base.txt", b"second\n", "second worker");
    handback_started_workspace(&repo, &second);
    let before = git_stdout(&repo.root, ["rev-parse", "HEAD"]);
    let conflict = integrate_started_workspace(&repo, &roots, &second);
    assert_eq!(conflict.outcome, "needs_user_action");
    assert_eq!(conflict.pre_integration_head, before);
    assert_eq!(conflict.post_integration_head, before);
    assert_eq!(conflict.conflict_paths, vec!["base.txt"]);
    assert!(conflict.integration_commits.is_empty());
    assert_eq!(git_stdout(&repo.root, ["rev-parse", "HEAD"]), before);
    assert_eq!(git_stdout(&repo.root, ["status", "--porcelain"]), "");
    assert_eq!(
        manifest_fields(&second).0,
        WorkspaceState::IntegrationBlocked
    );

    let replay_error = integrate_started_workspace_result(&repo, &roots, &second).unwrap_err();
    assert!(
        replay_error.contains("IntegrationBlocked") || replay_error.contains("expected state"),
        "conflict replay was not closed: {replay_error}"
    );
    let second_cleanup =
        abort_started_workspace(&repo, &roots, second, "integration conflict test cleanup");
    assert!(second_cleanup.lease_released);
    let first_cleanup = finish_started_workspace(&repo, &roots, first);
    assert!(first_cleanup.lease_released);
}

#[test]
fn cleanup_refuses_dirty_or_unretained_work() {
    if !cfg!(target_os = "linux") {
        eprintln!("SKIP native-only: writable workspace custody requires Linux");
        return;
    }
    let repo = TestRepository::init("dirty-retention");
    let roots = isolated_authority_roots(&repo, "dirty-retention");
    let started = start_test_workspace(
        &repo,
        &roots,
        "dirty",
        vec![claim("base.txt", "file")],
        None,
    );
    let worktree = std::path::Path::new(&started.result.worktree_root);
    fs::write(worktree.join("base.txt"), b"dirty and uncommitted\n").unwrap();
    let handback = handback_started_workspace_output(&repo, &started);
    assert!(
        !handback.status.success(),
        "dirty workspace handback unexpectedly succeeded"
    );
    assert!(
        String::from_utf8_lossy(&handback.stderr).contains("workspace is dirty at handback"),
        "wrong dirty handback refusal: {}",
        String::from_utf8_lossy(&handback.stderr)
    );
    assert_eq!(manifest_fields(&started).0, WorkspaceState::Running);
    let resource_path = started.resources.allocations[0].value.clone();
    let manifest_file = started.manifest_file.clone();
    let worker_capability_file = started.worker_capability_file.clone();

    let cleanup = abort_started_workspace(&repo, &roots, started, "retain dirty work");
    assert!(cleanup.retention_sha256.is_some());
    assert!(!cleanup.worktree_removed);
    assert!(!cleanup.branch_removed);
    assert!(cleanup.capabilities_revoked);
    assert!(cleanup.lease_released);
    assert_eq!(cleanup.resource_receipts.len(), 1);
    assert!(!std::path::Path::new(&resource_path).exists());
    assert!(
        manifest_file
            .with_file_name("retention-proof-v1.json")
            .is_file()
    );
    assert!(
        manifest_file
            .with_file_name("custody-lease-closed-v1.json")
            .is_file()
    );
    assert!(!worker_capability_file.exists());
    assert_eq!(
        manifest_fields_from_path(&manifest_file),
        WorkspaceState::Closed
    );

    let repo = TestRepository::init("unintegrated-retention");
    let roots = isolated_authority_roots(&repo, "unintegrated-retention");
    let started = start_test_workspace(
        &repo,
        &roots,
        "unintegrated",
        vec![claim("base.txt", "file")],
        None,
    );
    commit_workspace(
        &started,
        "base.txt",
        b"committed but unintegrated\n",
        "unintegrated worker",
    );
    handback_started_workspace(&repo, &started);
    let worktree = started.result.worktree_root.clone();
    let manifest_file = started.manifest_file.clone();
    let resource_path = started.resources.allocations[0].value.clone();
    let cleanup = abort_started_workspace(&repo, &roots, started, "retain unintegrated work");
    assert!(cleanup.retention_sha256.is_some());
    assert!(!cleanup.worktree_removed);
    assert!(!cleanup.branch_removed);
    assert!(
        Path::new(&worktree).exists(),
        "unintegrated worktree was deleted"
    );
    assert_eq!(
        manifest_fields_from_path(&manifest_file),
        WorkspaceState::Closed
    );
    assert!(
        manifest_file
            .with_file_name("retention-proof-v1.json")
            .is_file()
    );
    assert!(!Path::new(&resource_path).exists());

    let repo = TestRepository::init("identity-ambiguous-retention");
    let roots = isolated_authority_roots(&repo, "identity-ambiguous-retention");
    let started = start_test_workspace(
        &repo,
        &roots,
        "identity",
        vec![claim("base.txt", "file")],
        None,
    );
    commit_workspace(&started, "base.txt", b"identity\n", "identity worker");
    handback_started_workspace(&repo, &started);
    integrate_started_workspace(&repo, &roots, &started);
    let prepared = prepare_finish_request(&repo, &started);
    let manifest_path = started.manifest_file.clone();
    let manifest_bytes = fs::read(&manifest_path).unwrap();
    let mut manifest = parse_jcs(&manifest_bytes, true).unwrap().object().unwrap();
    let mut identity = manifest["worktree_identity"].clone().object().unwrap();
    identity.insert("inode".into(), JcsValue::String("0".into()));
    manifest.insert("worktree_identity".into(), JcsValue::Object(identity));
    fs::write(
        &manifest_path,
        format!(
            "{}\n",
            relay::workspace::schema::serialize_jcs(&JcsValue::Object(manifest))
        ),
    )
    .unwrap();
    let retained_worktree = started.result.worktree_root.clone();
    let retained_resource = started.resources.allocations[0].value.clone();
    let retained_capability = started.worker_capability_file.clone();
    drop(started.lease);
    let ambiguous = finish_prepared_workspace(&roots, &prepared).unwrap_err();
    assert!(
        ambiguous.contains("identity"),
        "wrong identity-ambiguity refusal: {ambiguous}"
    );
    assert!(Path::new(&retained_worktree).exists());
    assert_eq!(
        manifest_fields_from_path(&manifest_path),
        WorkspaceState::Integrated
    );
    assert!(Path::new(&retained_resource).exists());
    assert!(retained_capability.exists());
    fs::write(&manifest_path, manifest_bytes).unwrap();
    let cleanup = finish_prepared_workspace(&roots, &prepared).unwrap();
    assert!(cleanup.lease_released);

    let repo = TestRepository::init("unreceipted-retention");
    let roots = isolated_authority_roots(&repo, "unreceipted-retention");
    let started = start_test_workspace(
        &repo,
        &roots,
        "unreceipted",
        vec![claim("base.txt", "file")],
        None,
    );
    commit_workspace(&started, "base.txt", b"unreceipted\n", "unreceipted worker");
    handback_started_workspace(&repo, &started);
    integrate_started_workspace(&repo, &roots, &started);
    let prepared = prepare_finish_request(&repo, &started);
    let receipt_path = started
        .manifest_file
        .with_file_name("integration-receipt-v1.json");
    let hidden_receipt = repo.home.join("hidden-integration-receipt-v1.json");
    fs::rename(&receipt_path, &hidden_receipt).unwrap();
    let retained_worktree = started.result.worktree_root.clone();
    let retained_resource = started.resources.allocations[0].value.clone();
    let retained_capability = started.worker_capability_file.clone();
    let unreceipted_manifest = started.manifest_file.clone();
    drop(started.lease);
    let unreceipted = finish_prepared_workspace(&roots, &prepared).unwrap_err();
    assert!(
        unreceipted.contains("integration") || unreceipted.contains("receipt"),
        "wrong unreceipted refusal: {unreceipted}"
    );
    assert!(Path::new(&retained_worktree).exists());
    assert_eq!(
        manifest_fields_from_path(&unreceipted_manifest),
        WorkspaceState::Integrated
    );
    assert!(Path::new(&retained_resource).exists());
    assert!(retained_capability.exists());
    fs::rename(&hidden_receipt, &receipt_path).unwrap();
    let cleanup = finish_prepared_workspace(&roots, &prepared).unwrap();
    assert!(cleanup.lease_released);

    if std::env::var_os("SESSION_RELAY_TEST_CGROUP_ROOT").is_some() {
        for point in [
            "before_capability_revoke",
            "after_capability_revoke",
            "before_empty",
            "after_empty",
            "before_resources",
            "after_resources",
            "before_git",
            "after_git",
            "before_lease_close",
            "after_lease_close",
            "before_closed",
            "after_closed",
            "before_closed_committed",
            "after_closed_committed",
        ] {
            let repo = TestRepository::init(&format!("cleanup-fault-{point}"));
            let roots = isolated_authority_roots(&repo, &format!("cleanup-fault-{point}"));
            let task_slug = point.replace('_', "-");
            let started = start_test_workspace(
                &repo,
                &roots,
                &task_slug,
                vec![claim("base.txt", "file")],
                None,
            );
            commit_workspace(
                &started,
                "base.txt",
                format!("{point}\n").as_bytes(),
                &format!("worker {point}"),
            );
            handback_started_workspace(&repo, &started);
            integrate_started_workspace(&repo, &roots, &started);
            let prepared = prepare_finish_request(&repo, &started);
            let (_, broker_directory, custody_directory, cgroup) = runtime_artifact_paths(&started);
            let manifest_file = started.manifest_file.clone();
            let worktree = started.result.worktree_root.clone();
            let branch = started.result.branch_ref.clone();
            drop(started.lease);
            unsafe {
                std::env::set_var(
                    "SESSION_RELAY_TEST_CLEANUP_FAULT",
                    format!("{}:{point}", prepared.request.session_id),
                );
            }
            let injected = finish_prepared_workspace(&roots, &prepared).unwrap_err();
            unsafe {
                std::env::remove_var("SESSION_RELAY_TEST_CLEANUP_FAULT");
            }
            assert!(
                injected.contains("injected cleanup fault"),
                "{point}: wrong cleanup fault: {injected}"
            );
            let receipt = finish_prepared_workspace(&roots, &prepared).unwrap();
            let replay = finish_prepared_workspace(&roots, &prepared).unwrap();
            assert_eq!(replay, receipt, "{point}: cleanup replay changed receipt");
            assert_eq!(receipt.request_id, prepared.request.request_id);
            assert!(receipt.capabilities_revoked);
            assert!(receipt.lease_released);
            assert_eq!(receipt.resource_receipts.len(), 1);
            assert!(receipt.worktree_removed);
            assert!(receipt.branch_removed);
            assert!(!Path::new(&worktree).exists(), "{point}: worktree leaked");
            assert!(
                !git_output(&repo.root, ["show-ref", "--verify", "--quiet", &branch])
                    .status
                    .success(),
                "{point}: branch leaked"
            );
            assert!(
                !broker_directory.exists(),
                "{point}: Git broker socket directory leaked"
            );
            assert!(
                !custody_directory.exists(),
                "{point}: custody command socket directory leaked"
            );
            assert!(!cgroup.exists(), "{point}: delegated cgroup leaked");
            assert_eq!(
                fs::read(repo.root.join("base.txt")).unwrap(),
                format!("{point}\n").as_bytes(),
                "{point}: integrated user bytes were lost"
            );
            assert!(
                manifest_file
                    .with_file_name("custody-empty-v1.json")
                    .is_file()
            );
            assert!(
                manifest_file
                    .with_file_name("custody-lease-closed-v1.json")
                    .is_file()
            );
            assert_eq!(
                manifest_fields_from_path(&manifest_file),
                WorkspaceState::Closed
            );
            let mut releasing = 0;
            let mut closed = 0;
            for entry in fs::read_dir(manifest_file.with_file_name("journal")).unwrap() {
                let value = parse_jcs(&fs::read(entry.unwrap().path()).unwrap(), true).unwrap();
                let object = value.object().unwrap();
                match object["kind"].as_str().unwrap() {
                    "Releasing" => releasing += 1,
                    "Closed" => closed += 1,
                    _ => {}
                }
            }
            assert_eq!(releasing, 1, "{point}: duplicate/missing Releasing event");
            assert_eq!(closed, 1, "{point}: duplicate/missing Closed event");
        }
    }
}
