pub mod support;

use relay::workspace::authority::{AuthorityRoots, WorkspaceAuthority, WorkspaceLease};
use relay::workspace::capability::{
    authenticate_coordinator, authenticate_worker, mint_coordinator, mint_worker,
};
use relay::workspace::git::{
    OpenedRepository, PreserveResult, apply_wip, parse_name_status_z, preserve, provision_worktree,
    run_git_bytes, source_snapshot, validate_changed_paths,
};
use relay::workspace::repository_gate::RepositoryGate;
use relay::workspace::schema::{
    ClosedJcs, GitOid, HandbackReceiptV1, ObjectFormat, PathClaimRequestV1, PreserveRequestV1,
    WipPayloadV1, WipReceiptV1, WorkspaceState, parse_jcs, read_jcs_file,
};
use relay::workspace::{MANAGED_MUTATION_REFUSAL, preserve_workspace_with_roots};
use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use support::workspace::{
    TestRepository, abort_started_workspace, finish_started_workspace, git_ok, git_output,
    git_stdout, handback_started_workspace, handback_started_workspace_output,
    integrate_started_workspace, isolated_authority_roots, request_id, start_test_workspace,
    write_closed_record,
};

#[test]
fn schema_records_reject_noncanonical_unknown_and_duplicate_fields() {
    let canonical = br#"{"base_commit":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","created_at":"2026-07-22T12:34:56.789Z","label":"identity","mode":"commit","repository_path":"/tmp/repository","request_id":"11111111-1111-4111-8111-111111111111","schema":"1"}
"#;
    let value = parse_jcs(canonical, true).expect("canonical JCS request");
    let request = PreserveRequestV1::from_jcs(value).expect("closed preserve request");
    assert_eq!(request.mode, "commit");

    for invalid in [
        br#"{"schema":"1","request_id":"11111111-1111-4111-8111-111111111111","repository_path":"/tmp/repository","base_commit":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","mode":"commit","label":"identity","created_at":"2026-07-22T12:34:56.789Z"}
"#.as_slice(),
        br#"{"base_commit":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","created_at":"2026-07-22T12:34:56.789Z","extra":null,"label":"identity","mode":"commit","repository_path":"/tmp/repository","request_id":"11111111-1111-4111-8111-111111111111","schema":"1"}
"#.as_slice(),
        br#"{"base_commit":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","base_commit":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","created_at":"2026-07-22T12:34:56.789Z","label":"identity","mode":"commit","repository_path":"/tmp/repository","request_id":"11111111-1111-4111-8111-111111111111","schema":"1"}
"#.as_slice(),
        br#"{"base_commit":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","created_at":"2026-07-22T12:34:56.789Z","label":"identity","mode":"COMMIT","repository_path":"/tmp/repository","request_id":"11111111-1111-4111-8111-111111111111","schema":"1"}
"#.as_slice(),
        br#"{"base_commit":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","created_at":"2026-07-22T12:34:56.789Z","label":"identity","mode":"commit","repository_path":"/tmp/repository","request_id":"11111111-1111-4111-8111-111111111111","schema":"1"}
trailing"#.as_slice(),
    ] {
        assert!(
            parse_jcs(invalid, true)
                .and_then(PreserveRequestV1::from_jcs)
                .is_err(),
            "invalid record was admitted: {}",
            String::from_utf8_lossy(invalid)
        );
    }

    assert!(WorkspaceState::parse("running").is_err());
    assert!(WorkspaceState::Running.may_transition_to(WorkspaceState::HandbackReady));
    assert!(!WorkspaceState::Running.may_transition_to(WorkspaceState::Closed));
    assert!(
        !WorkspaceState::IntegrationBlocked.may_transition_to(WorkspaceState::IntegrationQueued)
    );
}

#[test]
fn symlink_relative_case_aliases_share_one_identity() {
    let repo = TestRepository::init("identity-alias");
    let direct = OpenedRepository::open(&repo.root).expect("open canonical repository");

    let alias = repo.home.join("repository-alias");
    symlink(&repo.root, &alias).unwrap();
    let through_symlink = OpenedRepository::open(&alias).expect("open supported symlink alias");
    let relative_shape = repo.root.join("subdir").join("..");
    fs::create_dir(repo.root.join("subdir")).unwrap();
    let through_relative =
        OpenedRepository::open(&relative_shape).expect("open relative-component alias");

    assert_eq!(through_symlink.identity, direct.identity);
    assert_eq!(through_relative.identity, direct.identity);
    assert!(through_symlink.common_dir_fd() >= 0);

    let case_shape = repo.home.join("SoUrCe");
    let through_case = match fs::canonicalize(&case_shape) {
        Ok(_) => {
            let opened = OpenedRepository::open(&case_shape).expect("open supported case alias");
            assert_eq!(opened.identity, direct.identity);
            Some(opened)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let upper = repo.home.join("CASE-ALIAS-PROBE");
            let lower = repo.home.join("case-alias-probe");
            fs::write(&upper, b"upper").unwrap();
            fs::write(&lower, b"lower").unwrap();
            let upper_metadata = fs::metadata(&upper).unwrap();
            let lower_metadata = fs::metadata(&lower).unwrap();
            assert_ne!(
                std::os::unix::fs::MetadataExt::ino(&upper_metadata),
                std::os::unix::fs::MetadataExt::ino(&lower_metadata),
                "case alias lookup failed even though the filesystem aliases case"
            );
            assert_eq!(fs::read(&upper).unwrap(), b"upper");
            assert_eq!(fs::read(&lower).unwrap(), b"lower");
            fs::remove_file(upper).unwrap();
            fs::remove_file(lower).unwrap();
            eprintln!("case aliases are explicitly unsupported on this case-sensitive filesystem");
            None
        }
        Err(error) => panic!("could not determine case-alias support: {error}"),
    };

    let lease_path = repo
        .home
        .join(format!("{}.lock", direct.identity.repository_id));
    let owner_path = repo
        .home
        .join(format!("{}.owner.json", direct.identity.repository_id));
    let owner = request_id();
    let lease =
        WorkspaceLease::acquire_owned(&lease_path, &owner_path, &owner, "2026-07-22T12:34:56.789Z")
            .unwrap();
    for opened in [&through_symlink, &through_relative]
        .into_iter()
        .chain(through_case.as_ref())
    {
        let refusal = match WorkspaceLease::acquire_owned(
            &repo
                .home
                .join(format!("{}.lock", opened.identity.repository_id)),
            &repo
                .home
                .join(format!("{}.owner.json", opened.identity.repository_id)),
            &request_id(),
            "2026-07-22T12:35:56.789Z",
        ) {
            Err(error) => error,
            Ok(_) => panic!("an alias acquired a second workspace lease"),
        };
        assert_eq!(
            refusal,
            format!(
                "Workspace already owned by session {owner}. Open a separate worktree or continue in read-only mode."
            )
        );
    }
    drop(lease);
}

#[test]
fn sha1_and_sha256_object_formats_validate_reported_oid_width() {
    assert_eq!(
        std::env::consts::OS,
        "linux",
        "A24 requires the promised native Linux workspace runtime"
    );
    for (format, expected, rejected) in [
        ("sha1", ObjectFormat::Sha1, "a".repeat(64)),
        ("sha256", ObjectFormat::Sha256, "a".repeat(40)),
    ] {
        let mut repo =
            TestRepository::init_with_object_format(&format!("object-format-{format}"), format);
        let probe = repo.home.join(format!("{format}-capability-probe"));
        let probe_output = git_output(
            &repo.root,
            [
                "init",
                "--quiet",
                &format!("--object-format={format}"),
                probe.to_str().unwrap(),
            ],
        );
        assert!(
            probe_output.status.success(),
            "Git promised {format} support is absent: {}",
            String::from_utf8_lossy(&probe_output.stderr)
        );
        assert_eq!(
            git_stdout(&probe, ["rev-parse", "--show-object-format"]),
            format
        );
        fs::remove_dir_all(&probe).unwrap();

        let opened = OpenedRepository::open(&repo.root).expect("open repository");
        assert_eq!(opened.identity.object_format, expected);
        assert_reported_oid(&repo.base_commit, expected.oid_len(), "preserve base");
        assert!(opened.validate_oid(&rejected).is_err());
        assert!(GitOid::parse(&"A".repeat(expected.oid_len()), expected).is_err());
        fs::write(repo.root.join("owned.txt"), b"initial\n").unwrap();
        git_ok(&repo.root, ["add", "--", "owned.txt"]);
        git_ok(&repo.root, ["commit", "--quiet", "-m", "owned fixture"]);
        repo.base_commit = git_stdout(&repo.root, ["rev-parse", "HEAD"]);
        assert_reported_oid(&repo.base_commit, expected.oid_len(), "preserve base");

        let roots = isolated_authority_roots(&repo, &format!("object-format-{format}"));
        let started = start_test_workspace(
            &repo,
            &roots,
            &format!("{format}-writer"),
            vec![claim("owned.txt", "file")],
            None,
        );
        let worktree = PathBuf::from(&started.result.worktree_root);
        fs::write(worktree.join("owned.txt"), format!("{format}\n")).unwrap();
        let shim = git_shim(&started);
        let add = Command::new(&shim)
            .args(["add", "--", "owned.txt"])
            .current_dir(&worktree)
            .output()
            .unwrap();
        assert!(
            add.status.success(),
            "broker add failed for {format}: {}",
            String::from_utf8_lossy(&add.stderr)
        );
        let commit = Command::new(&shim)
            .args(["commit", "-m", "object format worker"])
            .current_dir(&worktree)
            .output()
            .unwrap();
        assert!(
            commit.status.success(),
            "broker commit failed for {format}: {}",
            String::from_utf8_lossy(&commit.stderr)
        );
        let broker_oid = String::from_utf8(commit.stdout).unwrap().trim().to_owned();
        assert_reported_oid(&broker_oid, expected.oid_len(), "broker commit");
        assert_eq!(git_stdout(&worktree, ["rev-parse", "HEAD"]), broker_oid);

        handback_started_workspace(&repo, &started);
        let handback: HandbackReceiptV1 = read_jcs_file(
            &started
                .manifest_file
                .with_file_name("handback-receipt-v1.json"),
            None,
        )
        .unwrap();
        assert_eq!(handback.outcome, "validated");
        assert_reported_oid(&handback.head_oid, expected.oid_len(), "handback HEAD");
        for oid in &handback.produced_commits {
            assert_reported_oid(oid, expected.oid_len(), "handback produced commit");
        }

        let integration = integrate_started_workspace(&repo, &roots, &started);
        assert_eq!(integration.outcome, "integrated");
        assert_reported_oid(
            &integration.pre_integration_head,
            expected.oid_len(),
            "pre-integration HEAD",
        );
        assert_reported_oid(
            &integration.post_integration_head,
            expected.oid_len(),
            "post-integration HEAD",
        );
        for oid in integration
            .worker_commits
            .iter()
            .chain(&integration.integration_commits)
        {
            assert_reported_oid(oid, expected.oid_len(), "integration commit");
        }
        assert_eq!(
            git_stdout(&repo.root, ["rev-parse", "HEAD"]),
            integration.post_integration_head
        );
        let cleanup = finish_started_workspace(&repo, &roots, started);
        assert!(cleanup.lease_released);
    }
}

fn preserve_request(repo: &TestRepository, mode: &str, id: &str) -> PreserveRequestV1 {
    PreserveRequestV1 {
        request_id: id.to_owned(),
        repository_path: repo.root.to_string_lossy().into_owned(),
        base_commit: repo.base_commit.clone(),
        mode: mode.to_owned(),
        label: format!("{mode}-wip"),
        created_at: "2026-07-22T12:34:56.789Z".to_owned(),
    }
}

fn claim(path: &str, path_type: &str) -> PathClaimRequestV1 {
    PathClaimRequestV1 {
        path: path.to_owned(),
        path_type: path_type.to_owned(),
        mode: "exclusive".to_owned(),
    }
}

fn assert_reported_oid(oid: &str, expected_len: usize, label: &str) {
    assert_eq!(oid.len(), expected_len, "{label} has the wrong OID width");
    assert!(
        oid.bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "{label} is not repository-reported lowercase hexadecimal: {oid}"
    );
}

fn git_shim(started: &relay::workspace::StartedWorkspace) -> PathBuf {
    started
        .worker_capability_file
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("bin/git")
}

fn directory_snapshot(root: &Path) -> Vec<(String, Vec<u8>)> {
    fn visit(root: &Path, path: &Path, entries: &mut Vec<(String, Vec<u8>)>) {
        let mut children = fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        children.sort();
        for child in children {
            let relative = child
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            let metadata = fs::symlink_metadata(&child).unwrap();
            if metadata.is_dir() {
                entries.push((format!("{relative}/"), Vec::new()));
                visit(root, &child, entries);
            } else if metadata.file_type().is_symlink() {
                entries.push((
                    relative,
                    fs::read_link(&child)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned()
                        .into_bytes(),
                ));
            } else {
                entries.push((relative, fs::read(&child).unwrap()));
            }
        }
    }

    if !root.exists() {
        return Vec::new();
    }
    let mut entries = Vec::new();
    visit(root, root, &mut entries);
    entries
}

fn exercise_wip_mode(mode: &str, id: &str) {
    let repo = TestRepository::init(&format!("preserve-{mode}"));
    fs::write(repo.root.join("tracked.txt"), format!("{mode} tracked\n")).unwrap();
    fs::write(
        repo.root.join(format!("{mode}-untracked.txt")),
        b"untracked\n",
    )
    .unwrap();
    let opened = OpenedRepository::open(&repo.root).unwrap();
    let before = source_snapshot(&opened).unwrap();
    let request = preserve_request(&repo, mode, id);
    let preserved = preserve(
        &opened,
        &request,
        &"a".repeat(64),
        &repo.home.join(format!("{mode}-preserved")),
    )
    .unwrap();
    assert_eq!(source_snapshot(&opened).unwrap(), before);
    assert_eq!(preserved.receipt.before, preserved.receipt.after);
    let round_trip =
        read_jcs_file::<WipReceiptV1>(&preserved.receipt_file, Some(&preserved.receipt_sha256))
            .unwrap();
    assert_eq!(round_trip, preserved.receipt);

    match &preserved.receipt.payload {
        WipPayloadV1::Commit {
            preserved_commit,
            preserve_ref,
            ..
        } => {
            assert_eq!(mode, "commit");
            assert!(
                preserved
                    .receipt_file
                    .parent()
                    .unwrap()
                    .join("temporary-index")
                    .is_file()
            );
            assert_eq!(
                git_stdout(&repo.root, ["rev-parse", preserve_ref]),
                *preserved_commit
            );
        }
        WipPayloadV1::Artifact {
            binary_diff,
            untracked_inventory,
            untracked_archive,
            archive_format,
            ..
        } => {
            assert_eq!(mode, "artifact");
            assert_eq!(archive_format, "pax");
            for path in [binary_diff, untracked_inventory, untracked_archive] {
                assert!(std::path::Path::new(path).is_file());
            }
        }
    }

    let workspace = repo.home.join(format!("{mode}-workspace"));
    let branch_ref = format!("refs/heads/session-relay/{}/wip", request.request_id);
    let identity = provision_worktree(
        &opened,
        &workspace,
        &branch_ref,
        &request.request_id,
        "wip",
        &repo.base_commit,
    )
    .unwrap();
    assert_eq!(identity.branch_ref, branch_ref);
    let applied = apply_wip(
        &opened,
        &workspace,
        &branch_ref,
        &repo.base_commit,
        &preserved.receipt,
        "2026-07-22T12:35:56.789Z",
    )
    .unwrap();
    assert_eq!(git_stdout(&workspace, ["rev-parse", "HEAD"]), applied);
    assert_eq!(
        fs::read_to_string(workspace.join("tracked.txt")).unwrap(),
        format!("{mode} tracked\n")
    );
    assert_eq!(
        fs::read_to_string(workspace.join(format!("{mode}-untracked.txt"))).unwrap(),
        "untracked\n"
    );
    assert!(git_stdout(&workspace, ["status", "--porcelain"]).is_empty());
    assert!(
        apply_wip(
            &opened,
            &workspace,
            &branch_ref,
            &repo.base_commit,
            &preserved.receipt,
            "2026-07-22T12:36:56.789Z",
        )
        .unwrap_err()
        .contains("HEAD changed")
    );
    assert_eq!(source_snapshot(&opened).unwrap(), before);
}

#[test]
fn preserve_commit_mode_uses_temp_index_ref_and_no_source_mutation() {
    exercise_wip_mode("commit", &request_id());
}

fn artifact_preserve_refusal(repo: &TestRepository, expected: &str) -> Result<(), String> {
    let opened = OpenedRepository::open(&repo.root)?;
    let before = source_snapshot(&opened)?;
    let request = preserve_request(repo, "artifact", &request_id());
    let refusal = preserve(
        &opened,
        &request,
        &"f".repeat(64),
        &repo.home.join("preserved"),
    );
    let error = match refusal {
        Err(error) => error,
        Ok(result) => {
            return Err(format!(
                "artifact preserve unexpectedly succeeded with entries {:?}",
                match result.receipt.payload {
                    WipPayloadV1::Artifact { entries, .. } => entries,
                    _ => Vec::new(),
                }
            ));
        }
    };
    if !error.contains(expected) {
        return Err(format!("wrong artifact refusal: {error}"));
    }
    if source_snapshot(&opened)? != before {
        return Err("artifact refusal mutated source Git".into());
    }
    Ok(())
}

#[test]
fn preserve_artifact_mode_round_trips_binary_and_untracked_pax() {
    exercise_wip_mode("artifact", &request_id());
    let mut refusal_failures = Vec::new();

    {
        let mut repo = TestRepository::init("artifact-binary-roundtrip");
        let tracked = [0, 1, 2, 0xff, b'\n'];
        let untracked = [0xff, 0, 0x7f, b'\n'];
        fs::write(repo.root.join("tracked.bin"), b"binary baseline\n").unwrap();
        git_ok(&repo.root, ["add", "--", "tracked.bin"]);
        git_ok(&repo.root, ["commit", "--quiet", "-m", "binary baseline"]);
        repo.base_commit = git_stdout(&repo.root, ["rev-parse", "HEAD"]);
        fs::write(repo.root.join("tracked.bin"), tracked).unwrap();
        fs::write(repo.root.join("untracked.bin"), untracked).unwrap();
        let opened = OpenedRepository::open(&repo.root).unwrap();
        let request = preserve_request(&repo, "artifact", &request_id());
        let preserved = preserve(
            &opened,
            &request,
            &"f".repeat(64),
            &repo.home.join("preserved"),
        )
        .unwrap();
        let workspace = repo.home.join("binary-workspace");
        let branch_ref = format!("refs/heads/session-relay/{}/binary", request.request_id);
        provision_worktree(
            &opened,
            &workspace,
            &branch_ref,
            &request.request_id,
            "binary",
            &repo.base_commit,
        )
        .unwrap();
        apply_wip(
            &opened,
            &workspace,
            &branch_ref,
            &repo.base_commit,
            &preserved.receipt,
            "2026-07-22T12:35:56.789Z",
        )
        .unwrap();
        assert_eq!(fs::read(workspace.join("tracked.bin")).unwrap(), tracked);
        assert_eq!(
            fs::read(workspace.join("untracked.bin")).unwrap(),
            untracked
        );
        assert!(git_stdout(&workspace, ["status", "--porcelain"]).is_empty());
    }

    {
        let repo = TestRepository::init("artifact-traversal-refusal");
        symlink("../escape", repo.root.join("escape-link")).unwrap();
        if let Err(error) = artifact_preserve_refusal(&repo, "escapes the source root") {
            refusal_failures.push(format!("unsafe symlink/traversal: {error}"));
        }
    }

    {
        let repo = TestRepository::init("artifact-hardlink-refusal");
        fs::write(repo.root.join("unsafe.txt"), b"unsafe\n").unwrap();
        fs::hard_link(
            repo.root.join("unsafe.txt"),
            repo.root.join("unsafe-link.txt"),
        )
        .unwrap();
        if let Err(error) = artifact_preserve_refusal(&repo, "hard-linked") {
            refusal_failures.push(format!("hard link: {error}"));
        }
    }

    {
        let repo = TestRepository::init("artifact-fifo-type-refusal");
        let output = Command::new("mkfifo")
            .arg(repo.root.join("unsafe.fifo"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "mkfifo failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        if let Err(error) = artifact_preserve_refusal(&repo, "unsupported type") {
            refusal_failures.push(format!("FIFO type drift: {error}"));
        }
    }

    {
        let repo = TestRepository::init("artifact-socket-type-refusal");
        let socket_path = PathBuf::from(std::env::var("HOME").unwrap())
            .join(format!(".sr-{}.sock", request_id()));
        let socket = UnixListener::bind(&socket_path).unwrap();
        fs::rename(&socket_path, repo.root.join("unsafe.sock")).unwrap();
        if let Err(error) = artifact_preserve_refusal(&repo, "unsupported type") {
            refusal_failures.push(format!("socket type drift: {error}"));
        }
        drop(socket);
    }

    {
        let repo = TestRepository::init("artifact-device-type-refusal");
        let device = repo.root.join("unsafe.device");
        let output = Command::new("sudo")
            .args(["-n", "mknod", device.to_str().unwrap(), "c", "1", "3"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "promised native device-node test support is absent: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        if let Err(error) = artifact_preserve_refusal(&repo, "unsupported type") {
            refusal_failures.push(format!("device type drift: {error}"));
        }
    }

    let repo = TestRepository::init("artifact-duplicate-hash-refusal");
    fs::write(repo.root.join("payload.txt"), b"payload\n").unwrap();
    let opened = OpenedRepository::open(&repo.root).unwrap();
    let request = preserve_request(&repo, "artifact", &request_id());
    let preserved = preserve(
        &opened,
        &request,
        &"f".repeat(64),
        &repo.home.join("preserved"),
    )
    .unwrap();
    let archive = match &preserved.receipt.payload {
        WipPayloadV1::Artifact {
            untracked_archive, ..
        } => PathBuf::from(untracked_archive),
        _ => panic!("artifact payload"),
    };
    let mut traversal = preserved.receipt.clone();
    let WipPayloadV1::Artifact { entries, .. } = &mut traversal.payload else {
        panic!("artifact payload");
    };
    entries[0] = "../escape".into();
    let traversal_parse_error = WipReceiptV1::from_jcs(traversal.to_jcs()).unwrap_err();
    assert!(
        traversal_parse_error.contains("relative path"),
        "traversal PAX receipt was not rejected at its persisted boundary: {traversal_parse_error}"
    );
    let mut duplicate = preserved.receipt.clone();
    let WipPayloadV1::Artifact { entries, .. } = &mut duplicate.payload else {
        panic!("artifact payload");
    };
    entries.push(entries[0].clone());
    let duplicate_parse_error = WipReceiptV1::from_jcs(duplicate.to_jcs()).unwrap_err();
    assert!(
        duplicate_parse_error.contains("sorted and unique"),
        "duplicate PAX receipt was not rejected at its persisted boundary: {duplicate_parse_error}"
    );
    let duplicate_workspace = repo.home.join("duplicate-workspace");
    let duplicate_ref = format!("refs/heads/session-relay/{}/duplicate", request.request_id);
    provision_worktree(
        &opened,
        &duplicate_workspace,
        &duplicate_ref,
        &request.request_id,
        "duplicate",
        &repo.base_commit,
    )
    .unwrap();
    let duplicate_before =
        source_snapshot(&OpenedRepository::open(&duplicate_workspace).unwrap()).unwrap();
    let duplicate_error = apply_wip(
        &opened,
        &duplicate_workspace,
        &duplicate_ref,
        &repo.base_commit,
        &duplicate,
        "2026-07-22T12:35:56.789Z",
    )
    .unwrap_err();
    assert!(
        duplicate_error.contains("inventory differs"),
        "duplicate PAX inventory was not rejected: {duplicate_error}"
    );
    assert_eq!(
        source_snapshot(&OpenedRepository::open(&duplicate_workspace).unwrap()).unwrap(),
        duplicate_before,
        "duplicate PAX refusal mutated workspace Git"
    );

    fs::write(&archive, b"hash drift\n").unwrap();
    let hash_workspace = repo.home.join("hash-workspace");
    let hash_ref = format!("refs/heads/session-relay/{}/hash", request.request_id);
    provision_worktree(
        &opened,
        &hash_workspace,
        &hash_ref,
        &request.request_id,
        "hash",
        &repo.base_commit,
    )
    .unwrap();
    let hash_before = source_snapshot(&OpenedRepository::open(&hash_workspace).unwrap()).unwrap();
    let hash_error = apply_wip(
        &opened,
        &hash_workspace,
        &hash_ref,
        &repo.base_commit,
        &preserved.receipt,
        "2026-07-22T12:35:56.789Z",
    )
    .unwrap_err();
    assert!(
        hash_error.contains("content digest differs"),
        "PAX hash drift was not rejected: {hash_error}"
    );
    assert_eq!(
        source_snapshot(&OpenedRepository::open(&hash_workspace).unwrap()).unwrap(),
        hash_before,
        "PAX hash refusal mutated workspace Git"
    );
    assert!(
        refusal_failures.is_empty(),
        "artifact refusal gaps:\n{}",
        refusal_failures.join("\n")
    );
}

#[test]
fn mixed_changes_apply_index_then_worktree_payload() {
    let repo = TestRepository::init("mixed-wip-order");
    fs::write(repo.root.join("tracked.txt"), b"staged version\n").unwrap();
    git_ok(&repo.root, ["add", "tracked.txt"]);
    fs::write(repo.root.join("tracked.txt"), b"working-tree version\n").unwrap();
    fs::write(repo.root.join("new.txt"), b"untracked version\n").unwrap();
    let opened = OpenedRepository::open(&repo.root).unwrap();
    let before = source_snapshot(&opened).unwrap();
    let request = preserve_request(&repo, "artifact", &request_id());
    let preserved = preserve(
        &opened,
        &request,
        &"b".repeat(64),
        &repo.home.join("mixed-preserved"),
    )
    .unwrap();
    let workspace = repo.home.join("mixed-workspace");
    let branch_ref = format!("refs/heads/session-relay/{}/mixed", request.request_id);
    provision_worktree(
        &opened,
        &workspace,
        &branch_ref,
        &request.request_id,
        "mixed",
        &repo.base_commit,
    )
    .unwrap();
    apply_wip(
        &opened,
        &workspace,
        &branch_ref,
        &repo.base_commit,
        &preserved.receipt,
        "2026-07-22T12:35:56.789Z",
    )
    .unwrap();
    assert_eq!(
        fs::read_to_string(workspace.join("tracked.txt")).unwrap(),
        "working-tree version\n"
    );
    assert_eq!(
        fs::read_to_string(workspace.join("new.txt")).unwrap(),
        "untracked version\n"
    );
    assert_eq!(source_snapshot(&opened).unwrap(), before);
}

fn prepared_apply_case(
    tag: &str,
) -> (
    TestRepository,
    OpenedRepository,
    PreserveResult,
    PathBuf,
    String,
    PathBuf,
) {
    let repo = TestRepository::init(tag);
    fs::write(repo.root.join("tracked.txt"), b"preserved\n").unwrap();
    let opened = OpenedRepository::open(&repo.root).unwrap();
    let request = preserve_request(&repo, "commit", &request_id());
    let preserved = preserve(
        &opened,
        &request,
        &"d".repeat(64),
        &repo.home.join("preserved"),
    )
    .unwrap();
    let workspace = repo.home.join("workspace");
    let branch_ref = format!("refs/heads/session-relay/{}/drift", request.request_id);
    provision_worktree(
        &opened,
        &workspace,
        &branch_ref,
        &request.request_id,
        "drift",
        &repo.base_commit,
    )
    .unwrap();
    let journal = repo.home.join("session/journal");
    fs::create_dir_all(&journal).unwrap();
    fs::write(
        journal.join("00000000000000000001.json"),
        b"durable journal sentinel\n",
    )
    .unwrap();
    (repo, opened, preserved, workspace, branch_ref, journal)
}

struct ApplyRefusalCase<'a> {
    repo: &'a TestRepository,
    opened: &'a OpenedRepository,
    workspace: &'a Path,
    branch_ref: &'a str,
    base_commit: &'a str,
    receipt: &'a WipReceiptV1,
    journal: &'a Path,
    expected: &'a str,
}

fn assert_apply_refusal_without_mutation(case: ApplyRefusalCase<'_>) {
    let ApplyRefusalCase {
        repo,
        opened,
        workspace,
        branch_ref,
        base_commit,
        receipt,
        journal,
        expected,
    } = case;
    let source_before = repo.snapshot();
    let workspace_opened = OpenedRepository::open(workspace).unwrap();
    let workspace_before = source_snapshot(&workspace_opened).unwrap();
    let journal_before = directory_snapshot(journal);
    let error = apply_wip(
        opened,
        workspace,
        branch_ref,
        base_commit,
        receipt,
        "2026-07-22T12:35:56.789Z",
    )
    .unwrap_err();
    assert!(error.contains(expected), "wrong drift refusal: {error}");
    assert_eq!(
        repo.snapshot(),
        source_before,
        "drift refusal mutated source Git"
    );
    assert_eq!(
        source_snapshot(&workspace_opened).unwrap(),
        workspace_before,
        "drift refusal mutated workspace Git"
    );
    assert_eq!(
        directory_snapshot(journal),
        journal_before,
        "drift refusal appended or rewrote the durable journal"
    );
}

#[test]
fn unexpected_head_or_base_drift_is_refused() {
    {
        let (repo, opened, preserved, workspace, branch_ref, journal) =
            prepared_apply_case("exact-head-drift");
        git_ok(
            &workspace,
            ["commit", "--allow-empty", "-m", "unexpected HEAD"],
        );
        assert_apply_refusal_without_mutation(ApplyRefusalCase {
            repo: &repo,
            opened: &opened,
            workspace: &workspace,
            branch_ref: &branch_ref,
            base_commit: &repo.base_commit,
            receipt: &preserved.receipt,
            journal: &journal,
            expected: "HEAD changed",
        });
    }

    {
        let (repo, opened, preserved, workspace, branch_ref, journal) =
            prepared_apply_case("exact-base-drift");
        git_ok(
            &workspace,
            ["commit", "--allow-empty", "-m", "different base"],
        );
        let drifted_base = git_stdout(&workspace, ["rev-parse", "HEAD"]);
        assert_apply_refusal_without_mutation(ApplyRefusalCase {
            repo: &repo,
            opened: &opened,
            workspace: &workspace,
            branch_ref: &branch_ref,
            base_commit: &drifted_base,
            receipt: &preserved.receipt,
            journal: &journal,
            expected: "preserved commit parent differs",
        });
    }

    {
        let (repo, opened, preserved, workspace, branch_ref, journal) =
            prepared_apply_case("object-format-drift");
        let mut drifted_receipt = preserved.receipt.clone();
        drifted_receipt.repository.object_format = ObjectFormat::Sha256;
        assert_apply_refusal_without_mutation(ApplyRefusalCase {
            repo: &repo,
            opened: &opened,
            workspace: &workspace,
            branch_ref: &branch_ref,
            base_commit: &repo.base_commit,
            receipt: &drifted_receipt,
            journal: &journal,
            expected: "repository provenance differs",
        });
    }

    {
        let mut repo = TestRepository::init("ancestry-drift");
        let branch = git_stdout(&repo.root, ["symbolic-ref", "--short", "HEAD"]);
        git_ok(
            &repo.root,
            ["commit", "--allow-empty", "-m", "source descendant"],
        );
        let source_head = git_stdout(&repo.root, ["rev-parse", "HEAD"]);
        git_ok(
            &repo.root,
            ["checkout", "--quiet", "--detach", &repo.base_commit],
        );
        git_ok(
            &repo.root,
            ["commit", "--allow-empty", "-m", "unrelated sibling"],
        );
        let unrelated_base = git_stdout(&repo.root, ["rev-parse", "HEAD"]);
        git_ok(&repo.root, ["checkout", "--quiet", &branch]);
        assert_eq!(git_stdout(&repo.root, ["rev-parse", "HEAD"]), source_head);
        let opened = OpenedRepository::open(&repo.root).unwrap();
        let mut request = preserve_request(&repo, "commit", &request_id());
        request.base_commit = unrelated_base;
        let preserve_root = repo.home.join("ancestry-preserved");
        let journal = repo.home.join("session/journal");
        fs::create_dir_all(&journal).unwrap();
        fs::write(
            journal.join("00000000000000000001.json"),
            b"durable journal sentinel\n",
        )
        .unwrap();
        let source_before = repo.snapshot();
        let journal_before = directory_snapshot(&journal);
        let error = preserve(&opened, &request, &"e".repeat(64), &preserve_root).unwrap_err();
        assert!(
            error.contains("not an ancestor"),
            "wrong ancestry refusal: {error}"
        );
        assert_eq!(
            repo.snapshot(),
            source_before,
            "ancestry refusal mutated Git"
        );
        assert_eq!(
            directory_snapshot(&journal),
            journal_before,
            "ancestry refusal mutated the durable journal"
        );
        assert!(
            !preserve_root.exists(),
            "ancestry refusal materialized preservation state"
        );
        repo.base_commit = source_head;
    }
}

fn assert_handback_refusal_without_mutation(
    repo: &TestRepository,
    started: &relay::workspace::StartedWorkspace,
    expected: &str,
) {
    let worktree = Path::new(&started.result.worktree_root);
    let workspace = OpenedRepository::open(worktree).unwrap();
    let source_before = repo.snapshot();
    let workspace_before = source_snapshot(&workspace).unwrap();
    let manifest_before = fs::read(&started.manifest_file).unwrap();
    let journal = started.manifest_file.parent().unwrap().join("journal");
    let journal_before = directory_snapshot(&journal);
    let output = handback_started_workspace_output(repo, started);
    assert!(
        !output.status.success(),
        "invalid handback unexpectedly succeeded"
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains(expected),
        "wrong handback refusal, expected {expected:?}: {stderr}"
    );
    assert_eq!(
        repo.snapshot(),
        source_before,
        "handback refusal mutated source Git"
    );
    assert_eq!(
        source_snapshot(&workspace).unwrap(),
        workspace_before,
        "handback refusal mutated workspace Git"
    );
    assert_eq!(
        fs::read(&started.manifest_file).unwrap(),
        manifest_before,
        "handback refusal mutated the manifest"
    );
    assert_eq!(
        directory_snapshot(&journal),
        journal_before,
        "handback refusal mutated the journal"
    );
    assert!(
        !started
            .manifest_file
            .with_file_name("handback-receipt-v1.json")
            .exists(),
        "handback refusal published a receipt"
    );
}

#[test]
fn unowned_dirty_path_blocks_handback() {
    for (tag, staged) in [("dirty-index", true), ("dirty-worktree", false)] {
        let mut repo = TestRepository::init(tag);
        fs::write(repo.root.join("src.txt"), b"owned fixture\n").unwrap();
        git_ok(&repo.root, ["add", "--", "src.txt"]);
        git_ok(&repo.root, ["commit", "--quiet", "-m", "owned fixture"]);
        repo.base_commit = git_stdout(&repo.root, ["rev-parse", "HEAD"]);
        let roots = isolated_authority_roots(&repo, tag);
        let started =
            start_test_workspace(&repo, &roots, tag, vec![claim("src.txt", "file")], None);
        let worktree = PathBuf::from(&started.result.worktree_root);
        if staged {
            fs::write(worktree.join("outside.txt"), b"staged outside claim\n").unwrap();
            git_ok(&worktree, ["add", "--", "outside.txt"]);
            assert!(
                !git_output(&worktree, ["diff", "--cached", "--quiet", "HEAD", "--"])
                    .status
                    .success(),
                "index fixture is not dirty"
            );
            assert!(
                git_output(&worktree, ["diff", "--quiet", "--"])
                    .status
                    .success(),
                "index-only fixture also dirtied the worktree"
            );
        } else {
            fs::write(worktree.join("base.txt"), b"dirty outside claim\n").unwrap();
            assert!(
                git_output(&worktree, ["diff", "--cached", "--quiet", "HEAD", "--"])
                    .status
                    .success(),
                "worktree-only fixture dirtied the index"
            );
            assert!(
                !git_output(&worktree, ["diff", "--quiet", "--"])
                    .status
                    .success(),
                "worktree fixture is not dirty"
            );
        }
        assert_handback_refusal_without_mutation(&repo, &started, "workspace is dirty at handback");
        let cleanup = abort_started_workspace(&repo, &roots, started, "dirty-path test cleanup");
        assert!(cleanup.lease_released);
    }

    for (kind, source, destination) in [
        ("rename", "outside.txt", "owned/endpoint.txt"),
        ("rename", "owned/source.txt", "outside.txt"),
        ("copy", "outside.txt", "owned/endpoint.txt"),
        ("copy", "owned/source.txt", "outside.txt"),
    ] {
        let tag = format!(
            "{kind}-{}-{}",
            source.replace(['.', '/'], "-"),
            destination.replace(['.', '/'], "-")
        );
        let mut repo = TestRepository::init(&tag);
        let fixture = (0..128).fold(String::new(), |mut fixture, line| {
            writeln!(fixture, "line {line:03}").unwrap();
            fixture
        });
        fs::create_dir_all(repo.root.join("owned")).unwrap();
        fs::write(repo.root.join("owned/.keep"), b"claim root\n").unwrap();
        fs::write(repo.root.join(source), &fixture).unwrap();
        git_ok(&repo.root, ["add", "--", source, "owned/.keep"]);
        git_ok(&repo.root, ["commit", "--quiet", "-m", "endpoint fixture"]);
        repo.base_commit = git_stdout(&repo.root, ["rev-parse", "HEAD"]);
        let roots = isolated_authority_roots(&repo, &tag);
        let started =
            start_test_workspace(&repo, &roots, &tag, vec![claim("owned", "directory")], None);
        let worktree = PathBuf::from(&started.result.worktree_root);
        if kind == "rename" {
            git_ok(&worktree, ["mv", "--", source, destination]);
        } else {
            fs::copy(worktree.join(source), worktree.join(destination)).unwrap();
            fs::write(
                worktree.join(source),
                format!("{fixture}source changed after copy\n"),
            )
            .unwrap();
            git_ok(&worktree, ["add", "--", source, destination]);
        }
        git_ok(
            &worktree,
            ["commit", "--quiet", "-m", &format!("{kind} endpoint")],
        );
        assert!(
            git_output(&worktree, ["diff", "--cached", "--quiet", "HEAD", "--"])
                .status
                .success()
                && git_output(&worktree, ["diff", "--quiet", "--"])
                    .status
                    .success()
                && git_stdout(&worktree, ["status", "--porcelain=v2"]).is_empty(),
            "committed endpoint fixture is not clean"
        );

        let manifest = parse_jcs(&fs::read(&started.manifest_file).unwrap(), true).unwrap();
        let base = manifest.object().unwrap()["worker_base_commit"]
            .as_str()
            .unwrap()
            .to_owned();
        let raw = run_git_bytes(
            &worktree,
            &[
                "diff",
                "--name-status",
                "-z",
                "--find-renames",
                "--find-copies",
                &format!("{base}..HEAD"),
            ],
        )
        .unwrap();
        assert!(
            raw.contains(&0),
            "real Git did not emit NUL-delimited status"
        );
        let changes = parse_name_status_z(&raw).unwrap();
        let prefix = if kind == "rename" { 'R' } else { 'C' };
        assert!(
            changes.iter().any(|change| {
                change.status.starts_with(prefix)
                    && change.source.as_deref() == Some(source)
                    && change.destination == destination
            }),
            "real Git did not report {kind} endpoints {source} -> {destination}: {changes:?}"
        );
        let endpoint_error =
            validate_changed_paths(&changes, &[claim("owned", "directory")]).unwrap_err();
        assert!(
            endpoint_error.contains("outside admitted claims"),
            "{kind} admitted unowned endpoint {source} -> {destination}"
        );
        assert_handback_refusal_without_mutation(&repo, &started, "outside admitted claims");
        let cleanup = abort_started_workspace(&repo, &roots, started, "endpoint test cleanup");
        assert!(cleanup.lease_released);
    }
}

#[test]
fn integration_checkout_refuses_supported_writer() {
    let repo = TestRepository::init("integration-checkout-refusal");
    let opened = OpenedRepository::open(&repo.root).unwrap();
    let roots = isolated_authority_roots(&repo, "integration-checkout-refusal");
    let authority = WorkspaceAuthority::new(roots.clone()).unwrap();
    authority
        .bootstrap_coordinator(&opened.identity, "2026-07-22T12:34:56.789Z")
        .unwrap();
    let gate = RepositoryGate::acquire(&roots, &opened.identity).unwrap();
    let marker = gate
        .publish_workspace_marker(
            &roots,
            &opened.identity,
            env!("CARGO_PKG_VERSION"),
            "2026-07-22T12:34:56.789Z",
        )
        .unwrap();
    drop(gate);

    let before = repo.snapshot();
    let marker_before = fs::read(&marker).unwrap();
    let authority_before = directory_snapshot(&roots.authority);
    let relay_home = repo.home.join("relay-home");
    fs::create_dir(&relay_home).unwrap();
    fs::set_permissions(&relay_home, fs::Permissions::from_mode(0o700)).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_relay"))
        .args([
            "spawn",
            repo.root.to_str().unwrap(),
            "--tool",
            "claude",
            "--timeout",
            "1",
            "--",
            "must refuse before launch",
        ])
        .current_dir(&repo.root)
        .env("AGENT_RELAY_HOME", &relay_home)
        .env("RELAY_SPAWN_CMD_CLAUDE", repo.home.join("must-not-run"))
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "supported writer unexpectedly launched"
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains(&format!("spawn {MANAGED_MUTATION_REFUSAL}")),
        "supported writer did not return the exact workspace-start remedy: {stderr}"
    );
    assert!(
        !stderr.contains("already exists"),
        "generic Git collision was substituted for the managed-writer remedy"
    );
    assert_eq!(
        repo.snapshot(),
        before,
        "writer refusal mutated integration Git"
    );
    assert_eq!(
        fs::read(marker).unwrap(),
        marker_before,
        "writer refusal rewrote marker"
    );
    assert_eq!(
        directory_snapshot(&roots.authority),
        authority_before,
        "writer refusal mutated workspace authority"
    );
}

#[test]
fn failed_preflight_or_lease_changes_no_source_bytes() {
    let repo = TestRepository::init("failed-preflight");
    fs::write(repo.root.join("tracked.txt"), b"dirty source\n").unwrap();
    let opened = OpenedRepository::open(&repo.root).unwrap();
    let before = repo.snapshot();
    let mut request = preserve_request(&repo, "commit", &request_id());
    request.base_commit = request.base_commit.to_ascii_uppercase();
    assert!(
        preserve(
            &opened,
            &request,
            &"e".repeat(64),
            &repo.home.join("must-not-exist"),
        )
        .is_err()
    );
    assert_eq!(repo.snapshot(), before);

    let mut wrong_root = preserve_request(&repo, "commit", &request_id());
    wrong_root.repository_path = repo.home.join("other").to_string_lossy().into_owned();
    assert!(
        preserve(
            &opened,
            &wrong_root,
            &"e".repeat(64),
            &repo.home.join("must-not-exist-either"),
        )
        .is_err()
    );
    assert_eq!(repo.snapshot(), before);

    let resources = repo.home.join("resources");
    fs::create_dir(&resources).unwrap();
    fs::write(resources.join("sentinel"), b"resource sentinel\n").unwrap();
    let resources_before = directory_snapshot(&resources);

    let (receipt_repo, receipt_opened, preserved, workspace, branch_ref, journal) =
        prepared_apply_case("invalid-preserve-receipt");
    let mut invalid_receipt = preserved.receipt.clone();
    invalid_receipt.repository.repository_id = "0".repeat(64);
    assert_apply_refusal_without_mutation(ApplyRefusalCase {
        repo: &receipt_repo,
        opened: &receipt_opened,
        workspace: &workspace,
        branch_ref: &branch_ref,
        base_commit: &receipt_repo.base_commit,
        receipt: &invalid_receipt,
        journal: &journal,
        expected: "repository provenance differs",
    });

    let platform_repo = TestRepository::init("platform-admission-refusal");
    let platform_request = preserve_request(&platform_repo, "commit", &request_id());
    let request_file = platform_repo.home.join("preserve-v1.json");
    let request_sha256 = write_closed_record(&request_file, &platform_request);
    let platform_authority = platform_repo.home.join("platform-authority");
    fs::create_dir(&platform_authority).unwrap();
    fs::set_permissions(&platform_authority, fs::Permissions::from_mode(0o700)).unwrap();
    let platform_data = PathBuf::from(format!("/dev/shm/session-relay-{}", request_id()));
    fs::create_dir(&platform_data).unwrap();
    fs::set_permissions(&platform_data, fs::Permissions::from_mode(0o700)).unwrap();
    let platform_roots = AuthorityRoots {
        authority: platform_authority,
        data: platform_data.clone(),
        euid: unsafe { libc::geteuid() },
    };
    let platform_opened = OpenedRepository::open(&platform_repo.root).unwrap();
    let _authority = WorkspaceAuthority::new(platform_roots.clone()).unwrap();
    drop(RepositoryGate::acquire(&platform_roots, &platform_opened.identity).unwrap());
    let platform_source_before = platform_repo.snapshot();
    let platform_authority_before = directory_snapshot(&platform_roots.authority);
    let platform_error =
        preserve_workspace_with_roots(&platform_roots, &request_file, &request_sha256).unwrap_err();
    assert!(
        platform_error.contains("ext4"),
        "non-ext4 platform admission returned the wrong refusal: {platform_error}"
    );
    assert_eq!(platform_repo.snapshot(), platform_source_before);
    assert_eq!(
        directory_snapshot(&platform_roots.authority),
        platform_authority_before,
        "platform refusal mutated authority"
    );
    fs::remove_dir_all(platform_data).unwrap();

    let repository_id = opened.identity.repository_id.clone();
    let (coordinator, coordinator_record) =
        mint_coordinator(&repository_id, 1, "2026-07-22T12:34:56.789Z").unwrap();
    assert!(
        authenticate_coordinator(
            &coordinator,
            &coordinator_record,
            &"1".repeat(64),
            1,
            "start",
        )
        .is_err(),
        "wrong-repository coordinator capability authenticated"
    );
    let session_id = request_id();
    let (worker, worker_record) = mint_worker(
        &repository_id,
        &session_id,
        1,
        &repo.home.join("broker.sock"),
        "2026-07-22T12:34:56.789Z",
        "2026-07-22T13:34:56.789Z",
    )
    .unwrap();
    assert!(
        authenticate_worker(
            &worker,
            &worker_record,
            &repository_id,
            &request_id(),
            1,
            "handback",
            "2026-07-22T12:35:56.789Z",
        )
        .is_err(),
        "wrong-session worker capability authenticated"
    );

    let lease_path = repo.home.join("losing-lease.lock");
    let owner_path = repo.home.join("losing-lease.owner.json");
    let winner = WorkspaceLease::acquire_owned(
        &lease_path,
        &owner_path,
        &request_id(),
        "2026-07-22T12:34:56.789Z",
    )
    .unwrap();
    let source_before_loss = repo.snapshot();
    let authority_before_loss = directory_snapshot(&repo.home);
    assert!(
        WorkspaceLease::acquire_owned(
            &lease_path,
            &owner_path,
            &request_id(),
            "2026-07-22T12:35:56.789Z",
        )
        .is_err(),
        "losing lease acquired authority"
    );
    assert_eq!(repo.snapshot(), source_before_loss);
    assert_eq!(directory_snapshot(&repo.home), authority_before_loss);
    drop(winner);
    assert_eq!(directory_snapshot(&resources), resources_before);
}
