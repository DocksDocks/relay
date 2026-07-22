use relay::workspace::authority::AuthorityRoots;
use relay::workspace::resources::executable_sha256;
use relay::workspace::schema::{
    AbortRequestV1, CleanupReceiptV1, ClosedJcs, FinishRequestV1, HandbackRequestV1,
    IntegrateRequestV1, IntegrationReceiptV1, JcsValue, PathClaimRequestV1, PreserveRequestV1,
    ResourceDecisionV1, ToolLaunchV1, WorkspaceStartRequestV1, WorkspaceState, jcs_sha256,
    parse_jcs, serialize_jcs,
};
use relay::workspace::{
    StartedWorkspace, abort_workspace_with_roots, finish_workspace_with_roots,
    integrate_workspace_with_roots, preserve_workspace_with_roots,
    start_workspace_with_roots_and_executable,
};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use super::fresh_home;

pub const OWNERSHIP_REFUSAL_SUFFIX: &str =
    ". Open a separate worktree or continue in read-only mode.";
pub const INTEGRATION_CHECKOUT_REFUSAL: &str =
    "Start a managed writer with `session-relay workspace start` from a separate worktree.";

#[derive(Debug)]
pub struct TestRepository {
    pub root: PathBuf,
    pub home: PathBuf,
    pub base_commit: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceSnapshot {
    pub head: String,
    pub status: Vec<u8>,
    pub index: Option<Vec<u8>>,
    pub refs: Vec<u8>,
    pub tree: String,
}

impl TestRepository {
    pub fn init(tag: &str) -> Self {
        Self::init_with_object_format(tag, "sha1")
    }

    pub fn init_with_object_format(tag: &str, object_format: &str) -> Self {
        let home = fresh_home(tag);
        fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
        let root = home.join("source");
        fs::create_dir(&root).unwrap();
        git_ok(
            &root,
            [
                OsStr::new("init"),
                OsStr::new("--quiet"),
                OsStr::new("--object-format"),
                OsStr::new(object_format),
            ],
        );
        git_ok(&root, ["config", "user.name", "Session Relay Test"]);
        git_ok(&root, ["config", "user.email", "relay@example.test"]);
        fs::write(root.join("base.txt"), b"base\n").unwrap();
        git_ok(&root, ["add", "--", "base.txt"]);
        git_ok(&root, ["commit", "--quiet", "-m", "base"]);
        let base_commit = git_stdout(&root, ["rev-parse", "HEAD"]);
        Self {
            root,
            home,
            base_commit,
        }
    }

    pub fn snapshot(&self) -> SourceSnapshot {
        let git_dir = self.root.join(".git");
        SourceSnapshot {
            head: git_stdout(&self.root, ["rev-parse", "HEAD"]),
            status: git_output(
                &self.root,
                ["status", "--porcelain=v2", "-z", "--untracked-files=all"],
            )
            .stdout,
            index: fs::read(git_dir.join("index")).ok(),
            refs: git_output(
                &self.root,
                ["for-each-ref", "--format=%(refname)%00%(objectname)%00"],
            )
            .stdout,
            tree: git_stdout(&self.root, ["write-tree"]),
        }
    }

    pub fn git_dir(&self) -> PathBuf {
        PathBuf::from(git_stdout(
            &self.root,
            ["rev-parse", "--path-format=absolute", "--git-common-dir"],
        ))
    }
}

impl Drop for TestRepository {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.home);
    }
}

pub fn git_output<I, S>(cwd: &Path, args: I) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("execute git")
}

pub fn git_ok<I, S>(cwd: &Path, args: I)
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = git_output(cwd, args);
    assert!(
        output.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

pub fn git_stdout<I, S>(cwd: &Path, args: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = git_output(cwd, args);
    assert!(
        output.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("git output is UTF-8")
        .trim()
        .to_owned()
}

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
    let metadata = fs::metadata(&canonical).expect("stat Relay test binary");
    assert!(
        metadata.is_file(),
        "Relay test binary must be a regular file"
    );
    assert_ne!(
        metadata.permissions().mode() & 0o111,
        0,
        "Relay test binary must be executable"
    );
    canonical
}

fn build_test_tool(repo: &TestRepository, task_slug: &str) -> PathBuf {
    let source = repo.home.join(format!("{task_slug}-worker-fixture.c"));
    let executable = repo.home.join(format!("{task_slug}-worker-fixture"));
    fs::write(
        &source,
        b"#include <signal.h>\n#include <unistd.h>\nint main(void) { for (;;) pause(); }\n",
    )
    .unwrap();
    let output = Command::new("cc")
        .args(["-x", "c", "-O0", "-o"])
        .arg(&executable)
        .arg(&source)
        .stdin(Stdio::null())
        .output()
        .expect("compile native test worker fixture");
    assert!(
        output.status.success(),
        "compile native test worker fixture: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o500)).unwrap();
    fs::canonicalize(executable).unwrap()
}

pub fn relay_output<I, S>(cwd: &Path, args: I) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    Command::new(relay_test_binary())
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()
        .expect("execute relay")
}

pub fn workspace_output<I, S>(cwd: &Path, args: I) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = vec![OsString::from("workspace")];
    command.extend(args.into_iter().map(|arg| arg.as_ref().to_owned()));
    relay_output(cwd, command)
}

pub fn wait_until(description: &str, timeout: Duration, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if condition() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for {description}");
}

pub fn assert_refused_without_mutation(
    repo: &TestRepository,
    before: &SourceSnapshot,
    output: &Output,
    message: &str,
) {
    assert!(!output.status.success(), "operation unexpectedly succeeded");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(message),
        "wrong refusal: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        &repo.snapshot(),
        before,
        "refusal mutated source repository"
    );
}

pub fn isolated_authority_roots(repo: &TestRepository, label: &str) -> AuthorityRoots {
    let authority = repo.home.join(format!("{label}-authority"));
    let data = repo.home.join(format!("{label}-data"));
    for root in [&authority, &data] {
        fs::create_dir(root).unwrap();
        fs::set_permissions(root, fs::Permissions::from_mode(0o700)).unwrap();
    }
    AuthorityRoots {
        authority,
        data,
        euid: unsafe { libc::geteuid() },
    }
}

pub fn write_closed_record<T: ClosedJcs>(path: &Path, record: &T) -> String {
    let mut bytes = serialize_jcs(&record.to_jcs()).into_bytes();
    bytes.push(b'\n');
    fs::write(path, &bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    jcs_sha256(record)
}

#[derive(Clone, Debug)]
pub struct PreparedWorkspaceStart {
    pub request: WorkspaceStartRequestV1,
    pub request_file: PathBuf,
    pub request_sha256: String,
    pub relay_executable: PathBuf,
}

pub fn prepare_test_workspace(
    repo: &TestRepository,
    roots: &AuthorityRoots,
    task_slug: &str,
    preserve_mode: &str,
    owned_paths: Vec<PathClaimRequestV1>,
) -> PreparedWorkspaceStart {
    let preserve = PreserveRequestV1 {
        request_id: request_id(),
        repository_path: repo.root.to_string_lossy().into_owned(),
        base_commit: repo.base_commit.clone(),
        mode: preserve_mode.to_owned(),
        label: format!("{task_slug}-wip"),
        created_at: "2026-07-22T12:34:56.789Z".to_owned(),
    };
    let preserve_file = repo.home.join(format!("{task_slug}-preserve-v1.json"));
    let preserve_sha256 = write_closed_record(&preserve_file, &preserve);
    let preserved = preserve_workspace_with_roots(roots, &preserve_file, &preserve_sha256).unwrap();

    let built_relay = relay_test_binary();
    let relay_executable = repo.home.join(format!("{task_slug}-fresh-relay"));
    fs::write(&relay_executable, fs::read(&built_relay).unwrap()).unwrap();
    fs::set_permissions(&relay_executable, fs::Permissions::from_mode(0o500)).unwrap();
    let relay_executable = fs::canonicalize(relay_executable).unwrap();
    let tool_executable = build_test_tool(repo, task_slug);
    let executable_sha256 = executable_sha256(&tool_executable).unwrap();
    let resources = [
        "port",
        "temp_dir",
        "build_dir",
        "database_schema",
        "log_dir",
        "cache_dir",
    ]
    .into_iter()
    .map(|kind| {
        let requested = kind == "temp_dir";
        ResourceDecisionV1 {
            kind: kind.to_owned(),
            name: kind.to_owned(),
            state: if requested { "requested" } else { "unused" }.to_owned(),
            provider_id: None,
            reason: (!requested).then(|| "task_does_not_use_resource".to_owned()),
        }
    })
    .collect();
    let request = WorkspaceStartRequestV1 {
        request_id: request_id(),
        repository_path: repo.root.to_string_lossy().into_owned(),
        integration_root: repo.root.to_string_lossy().into_owned(),
        base_commit: repo.base_commit.clone(),
        task_slug: task_slug.to_owned(),
        task: format!("test workspace {task_slug}"),
        tool: ToolLaunchV1 {
            kind: "omp".to_owned(),
            executable_path: tool_executable.to_string_lossy().into_owned(),
            executable_sha256,
            model: None,
            effort: None,
            service_tier: None,
        },
        wip_receipt_path: preserved.receipt_file.to_string_lossy().into_owned(),
        wip_receipt_sha256: preserved.receipt_sha256,
        owned_paths,
        coordinator_owned_paths: Vec::new(),
        coordinator_owned_overrides: Vec::<JcsValue>::new(),
        resources,
        created_at: "2026-07-22T12:35:56.789Z".to_owned(),
    };
    let request_file = repo.home.join(format!("{task_slug}-start-v1.json"));
    let request_sha256 = write_closed_record(&request_file, &request);
    PreparedWorkspaceStart {
        request,
        request_file,
        request_sha256,
        relay_executable,
    }
}

pub fn start_prepared_workspace(
    roots: &AuthorityRoots,
    prepared: &PreparedWorkspaceStart,
    coordinator_capability_file: Option<&Path>,
) -> Result<StartedWorkspace, String> {
    start_workspace_with_roots_and_executable(
        roots,
        &prepared.request_file,
        &prepared.request_sha256,
        coordinator_capability_file,
        &prepared.relay_executable,
    )
}

pub fn start_test_workspace_with_preservation_mode(
    repo: &TestRepository,
    roots: &AuthorityRoots,
    task_slug: &str,
    preserve_mode: &str,
    owned_paths: Vec<PathClaimRequestV1>,
    coordinator_capability_file: Option<&Path>,
) -> StartedWorkspace {
    let prepared = prepare_test_workspace(repo, roots, task_slug, preserve_mode, owned_paths);
    start_prepared_workspace(roots, &prepared, coordinator_capability_file).unwrap()
}

pub fn start_test_workspace(
    repo: &TestRepository,
    roots: &AuthorityRoots,
    task_slug: &str,
    owned_paths: Vec<PathClaimRequestV1>,
    coordinator_capability_file: Option<&Path>,
) -> StartedWorkspace {
    start_test_workspace_with_preservation_mode(
        repo,
        roots,
        task_slug,
        "commit",
        owned_paths,
        coordinator_capability_file,
    )
}

pub fn manifest_fields(started: &StartedWorkspace) -> (WorkspaceState, String, String) {
    let value = parse_jcs(&fs::read(&started.manifest_file).unwrap(), true).unwrap();
    let object = value.object().unwrap();
    (
        WorkspaceState::parse(object["state"].as_str().unwrap()).unwrap(),
        object["journal_head_sha256"].as_str().unwrap().to_owned(),
        object["worktree_root"].as_str().unwrap().to_owned(),
    )
}

pub fn abort_started_workspace(
    repo: &TestRepository,
    roots: &AuthorityRoots,
    started: StartedWorkspace,
    reason: &str,
) -> CleanupReceiptV1 {
    let (state, journal_head, worktree_root) = manifest_fields(&started);
    let expected_head = git_stdout(Path::new(&worktree_root), ["rev-parse", "HEAD"]);
    drop(started.lease);
    let request = AbortRequestV1 {
        request_id: request_id(),
        repository_path: repo.root.to_string_lossy().into_owned(),
        repository_id: started.result.repository_id.clone(),
        session_id: started.result.session_id.clone(),
        expected_state: state,
        expected_journal_head_sha256: journal_head,
        expected_head,
        reason: reason.to_owned(),
        created_at: "2026-07-22T12:59:56.789Z".to_owned(),
    };
    let request_file = repo
        .home
        .join(format!("{}-abort-v1.json", started.result.session_id));
    let request_sha256 = write_closed_record(&request_file, &request);
    abort_workspace_with_roots(
        roots,
        &request_file,
        &request_sha256,
        Path::new(&started.result.coordinator_capability_file),
    )
    .unwrap()
}

pub fn commit_workspace(
    started: &StartedWorkspace,
    relative: &str,
    contents: &[u8],
    message: &str,
) -> String {
    let worktree = Path::new(&started.result.worktree_root);
    let path = worktree.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, contents).unwrap();
    git_ok(worktree, ["add", "--", relative]);
    git_ok(worktree, ["commit", "--quiet", "-m", message]);
    git_stdout(worktree, ["rev-parse", "HEAD"])
}

pub fn handback_started_workspace_output(
    repo: &TestRepository,
    started: &StartedWorkspace,
) -> Output {
    let worktree = Path::new(&started.result.worktree_root);
    let request = HandbackRequestV1 {
        request_id: request_id(),
        session_id: started.result.session_id.clone(),
        expected_head: git_stdout(worktree, ["rev-parse", "HEAD"]),
        created_at: "2026-07-22T13:01:56.789Z".to_owned(),
    };
    let request_file = repo
        .home
        .join(format!("{}-handback-v1.json", started.result.session_id));
    let request_sha256 = write_closed_record(&request_file, &request);
    workspace_output(
        worktree,
        [
            OsStr::new("handback"),
            OsStr::new("--request-file"),
            request_file.as_os_str(),
            OsStr::new("--request-sha256"),
            OsStr::new(&request_sha256),
            OsStr::new("--worker-capability-file"),
            started.worker_capability_file.as_os_str(),
        ],
    )
}

pub fn handback_started_workspace(repo: &TestRepository, started: &StartedWorkspace) {
    let output = handback_started_workspace_output(repo, started);
    assert!(
        output.status.success(),
        "handback failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    wait_until("HandbackReady manifest", Duration::from_secs(10), || {
        manifest_fields(started).0 == WorkspaceState::HandbackReady
    });
}

#[derive(Clone, Debug)]
pub struct PreparedIntegrationRequest {
    pub request: IntegrateRequestV1,
    pub request_file: PathBuf,
    pub request_sha256: String,
    pub coordinator_capability_file: PathBuf,
}

pub fn prepare_integration_request(
    repo: &TestRepository,
    started: &StartedWorkspace,
) -> PreparedIntegrationRequest {
    let (state, journal_head, _) = manifest_fields(started);
    let request = IntegrateRequestV1 {
        request_id: request_id(),
        repository_path: repo.root.to_string_lossy().into_owned(),
        repository_id: started.result.repository_id.clone(),
        session_id: started.result.session_id.clone(),
        expected_state: state,
        expected_journal_head_sha256: journal_head,
        expected_head: git_stdout(&repo.root, ["rev-parse", "HEAD"]),
        disposition: "integrate".to_owned(),
        created_at: "2026-07-22T13:02:56.789Z".to_owned(),
    };
    let request_file = repo
        .home
        .join(format!("{}-integrate-v1.json", started.result.session_id));
    let request_sha256 = write_closed_record(&request_file, &request);
    PreparedIntegrationRequest {
        request,
        request_file,
        request_sha256,
        coordinator_capability_file: PathBuf::from(&started.result.coordinator_capability_file),
    }
}

pub fn integrate_prepared_workspace(
    roots: &AuthorityRoots,
    prepared: &PreparedIntegrationRequest,
) -> Result<IntegrationReceiptV1, String> {
    integrate_workspace_with_roots(
        roots,
        &prepared.request_file,
        &prepared.request_sha256,
        &prepared.coordinator_capability_file,
    )
}

pub fn integrate_started_workspace_result(
    repo: &TestRepository,
    roots: &AuthorityRoots,
    started: &StartedWorkspace,
) -> Result<IntegrationReceiptV1, String> {
    let prepared = prepare_integration_request(repo, started);
    integrate_prepared_workspace(roots, &prepared)
}

pub fn integrate_started_workspace(
    repo: &TestRepository,
    roots: &AuthorityRoots,
    started: &StartedWorkspace,
) -> IntegrationReceiptV1 {
    integrate_started_workspace_result(repo, roots, started).unwrap()
}

fn sha256_bytes(bytes: &[u8]) -> String {
    let mut child = Command::new("/usr/bin/sha256sum")
        .arg("--binary")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn /usr/bin/sha256sum");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(bytes)
        .expect("write bytes to sha256sum");
    let output = child.wait_with_output().expect("wait for sha256sum");
    assert!(
        output.status.success(),
        "sha256sum failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("sha256sum output is UTF-8");
    let digest = stdout
        .strip_suffix(" *-\n")
        .expect("canonical sha256sum --binary stdin output");
    assert!(
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "sha256sum produced a noncanonical digest"
    );
    digest.to_owned()
}

pub fn runtime_artifact_paths(started: &StartedWorkspace) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    let capability = parse_jcs(&fs::read(&started.worker_capability_file).unwrap(), true).unwrap();
    let broker_socket = PathBuf::from(
        capability.object().unwrap()["broker_socket"]
            .as_str()
            .unwrap(),
    );
    let custody = parse_jcs(
        &fs::read(
            started
                .manifest_file
                .with_file_name("custody-active-v1.json"),
        )
        .unwrap(),
        true,
    )
    .unwrap();
    let custody = custody.object().unwrap();
    let membership = custody["cgroup_membership"].as_str().unwrap();
    let session_dir = started.manifest_file.parent().unwrap();
    let custody_identity = sha256_bytes(
        [
            b"session-relay/custody-socket/v1\0".as_slice(),
            session_dir.as_os_str().as_bytes(),
        ]
        .concat()
        .as_slice(),
    );
    let custody_directory = Path::new(&format!("/tmp/sr-custody-{}", unsafe { libc::geteuid() }))
        .join(custody_identity);
    (
        PathBuf::from(&started.result.worktree_root),
        broker_socket.parent().unwrap().to_owned(),
        custody_directory,
        Path::new("/sys/fs/cgroup").join(membership.trim_start_matches('/')),
    )
}

#[derive(Clone, Debug)]
pub struct PreparedFinishRequest {
    pub request: FinishRequestV1,
    pub request_file: PathBuf,
    pub request_sha256: String,
    pub coordinator_capability_file: PathBuf,
}

pub fn prepare_finish_request(
    repo: &TestRepository,
    started: &StartedWorkspace,
) -> PreparedFinishRequest {
    let (state, journal_head, _) = manifest_fields(started);
    let request = FinishRequestV1 {
        request_id: request_id(),
        repository_path: repo.root.to_string_lossy().into_owned(),
        repository_id: started.result.repository_id.clone(),
        session_id: started.result.session_id.clone(),
        expected_state: state,
        expected_journal_head_sha256: journal_head,
        expected_head: git_stdout(&repo.root, ["rev-parse", "HEAD"]),
        acknowledge_needs_user_action: false,
        created_at: "2026-07-22T13:03:56.789Z".to_owned(),
    };
    let request_file = repo
        .home
        .join(format!("{}-finish-v1.json", started.result.session_id));
    let request_sha256 = write_closed_record(&request_file, &request);
    PreparedFinishRequest {
        request,
        request_file,
        request_sha256,
        coordinator_capability_file: PathBuf::from(&started.result.coordinator_capability_file),
    }
}

pub fn finish_prepared_workspace(
    roots: &AuthorityRoots,
    prepared: &PreparedFinishRequest,
) -> Result<CleanupReceiptV1, String> {
    finish_workspace_with_roots(
        roots,
        &prepared.request_file,
        &prepared.request_sha256,
        &prepared.coordinator_capability_file,
    )
}

pub fn finish_started_workspace(
    repo: &TestRepository,
    roots: &AuthorityRoots,
    started: StartedWorkspace,
) -> CleanupReceiptV1 {
    let (worktree_root, broker_directory, custody_directory, cgroup) =
        runtime_artifact_paths(&started);
    let prepared = prepare_finish_request(repo, &started);
    drop(started.lease);
    let cleanup = finish_prepared_workspace(roots, &prepared).unwrap();
    assert!(cleanup.worktree_removed);
    assert!(cleanup.branch_removed);
    assert!(cleanup.capabilities_revoked);
    assert!(cleanup.lease_released);
    assert!(!worktree_root.exists(), "cleanup left workspace worktree");
    assert!(
        !broker_directory.exists(),
        "cleanup left Git broker directory"
    );
    assert!(
        !custody_directory.exists(),
        "cleanup left custody command socket directory"
    );
    assert!(!cgroup.exists(), "cleanup left delegated cgroup");
    cleanup
}

pub fn request_id() -> String {
    relay::store::uuid_v4()
}
