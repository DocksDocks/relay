pub mod support;

use relay::fanout::{self, FanoutMode, FanoutStore};
use relay::lifecycle::{LifecycleStore, ProcessObservation, StartGeneration};
use relay::store;
use relay::workspace::authority::{
    AuthorityRootProvider, SystemAuthorityRootProvider, WorkspaceAuthority,
};
use relay::workspace::git::OpenedRepository;
use std::collections::HashMap;
use std::fs::{self, FileTimes};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, SystemTime};
use support::fanout::{activate_worker, mutate_fanout_record, seed_entry};
use support::fresh_home;
use tinyjson::JsonValue;

const GC_DAYS: u64 = 1;
const PARENT_SESSION_ID: &str = "11111111-1111-4111-8111-111111111111";

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("git output is UTF-8")
        .trim()
        .to_string()
}

fn init_repo(home: &Path) -> PathBuf {
    let repo = home.join("repo");
    fs::create_dir_all(&repo).expect("create repository");
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "relay@example.test"]);
    git(&repo, &["config", "user.name", "Relay Test"]);
    fs::write(repo.join("base.txt"), "base\n").expect("write base file");
    git(&repo, &["add", "base.txt"]);
    git(&repo, &["commit", "-qm", "base"]);
    repo
}

struct Fixture {
    home: PathBuf,
    repo: PathBuf,
    store: FanoutStore,
    reservation: fanout::FanoutRecord,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let home = fresh_home(tag);
        let repo = init_repo(&home);
        seed_entry(&home, PARENT_SESSION_ID, &repo);
        let store = FanoutStore::new(home.clone());
        let reservation =
            fanout::prepare_worktree(&store, &repo, PARENT_SESSION_ID, FanoutMode::Root)
                .expect("reserve fanout worktree");
        Self {
            home,
            repo,
            store,
            reservation,
        }
    }

    fn worktree(&self) -> &Path {
        Path::new(&self.reservation.worktree)
    }

    fn age_past_gc_window(&self) {
        let old = SystemTime::now() - Duration::from_secs((GC_DAYS + 1) * 24 * 60 * 60);
        let times = FileTimes::new().set_accessed(old).set_modified(old);
        fs::File::open(self.worktree())
            .expect("open worktree directory")
            .set_times(times)
            .expect("backdate worktree directory");
    }

    fn branch_exists(&self) -> bool {
        Command::new("git")
            .args([
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{}", self.reservation.branch),
            ])
            .current_dir(&self.repo)
            .status()
            .expect("inspect reservation branch")
            .success()
    }

    fn run_gc(&self) -> Output {
        self.run_gc_with_days(Some(GC_DAYS))
    }

    fn run_gc_with_default_retention(&self) -> Output {
        self.run_gc_with_days(None)
    }

    fn run_gc_with_days(&self, gc_days: Option<u64>) -> Output {
        let session_id = store::uuid_v4();
        let input = format!(
            r#"{{"session_id":"{session_id}","cwd":"{}","source":"startup"}}"#,
            self.repo.display()
        );
        let mut command = Command::new(env!("CARGO_BIN_EXE_relay"));
        command
            .arg("hook")
            .env("AGENT_RELAY_HOME", &self.home)
            .env_remove("SESSION_RELAY_HOME")
            .env_remove("AGENT_RELAY_GC_DAYS")
            .env("RELAY_NO_WATCH", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(days) = gc_days {
            command.env("AGENT_RELAY_GC_DAYS", days.to_string());
        }
        let mut child = command.spawn().expect("run relay hook");
        child
            .stdin
            .as_mut()
            .expect("hook stdin")
            .write_all(input.as_bytes())
            .expect("write hook input");
        let output = child.wait_with_output().expect("wait for relay hook");
        assert!(
            output.status.success(),
            "relay hook failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !String::from_utf8_lossy(&output.stderr).contains("GC skipped"),
            "relay GC failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.home).ok();
    }
}

struct RemoveDirOnDrop(PathBuf);

impl Drop for RemoveDirOnDrop {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).ok();
    }
}

fn set_only_operation_to_lost_authority(home: &Path) {
    let path = home.join("lifecycle-v1.json");
    let parsed = fs::read_to_string(&path)
        .expect("read lifecycle authority")
        .parse::<JsonValue>()
        .expect("parse lifecycle authority");
    let mut root = parsed
        .get::<HashMap<String, JsonValue>>()
        .expect("lifecycle authority envelope")
        .clone();
    let mut state = root["state"]
        .get::<HashMap<String, JsonValue>>()
        .expect("lifecycle authority state")
        .clone();
    let mut operations = state["active_operations"]
        .get::<HashMap<String, JsonValue>>()
        .expect("active operations")
        .clone();
    assert_eq!(operations.len(), 1, "expected one active operation");
    let operation_id = operations.keys().next().expect("operation id").clone();
    let mut operation = operations[&operation_id]
        .get::<HashMap<String, JsonValue>>()
        .expect("active operation")
        .clone();
    let custody = HashMap::from([
        (
            "kind".to_string(),
            JsonValue::from("LostAuthority".to_string()),
        ),
        ("last_observation".to_string(), JsonValue::from(())),
        (
            "reason".to_string(),
            JsonValue::from("SupervisorLost".to_string()),
        ),
    ]);
    operation.insert("custody".to_string(), JsonValue::from(custody));
    operations.insert(operation_id, JsonValue::from(operation));
    state.insert("active_operations".to_string(), JsonValue::from(operations));
    root.insert("state".to_string(), JsonValue::from(state));
    fs::write(
        path,
        JsonValue::from(root)
            .format()
            .expect("format lifecycle authority"),
    )
    .expect("write lifecycle authority");
}

#[test]
fn abandoned_reservation_past_window_removes_worktree_and_reports_retained_branch() {
    let fixture = Fixture::new("fanout-reap-abandoned");
    fixture.age_past_gc_window();

    let output = fixture.run_gc();
    let report = String::from_utf8_lossy(&output.stderr);

    assert!(!fixture.worktree().exists());
    assert!(fixture.branch_exists());
    assert!(report.contains(&fixture.reservation.branch), "{report}");
}

#[test]
fn reservation_inside_window_keeps_worktree_and_branch() {
    let fixture = Fixture::new("fanout-reap-fresh");

    let output = fixture.run_gc();
    let report = String::from_utf8_lossy(&output.stderr);

    assert!(fixture.worktree().is_dir());
    assert!(fixture.branch_exists());
    assert!(!report.contains(&fixture.reservation.branch), "{report}");
}

#[test]
fn reservation_with_uncollected_commits_keeps_worktree_and_reports_branch() {
    let fixture = Fixture::new("fanout-reap-ahead");
    fs::write(fixture.worktree().join("uncollected.txt"), "uncollected\n")
        .expect("write uncollected work");
    git(fixture.worktree(), &["add", "uncollected.txt"]);
    git(fixture.worktree(), &["commit", "-qm", "uncollected work"]);
    fixture.age_past_gc_window();

    let output = fixture.run_gc();
    let report = String::from_utf8_lossy(&output.stderr);

    assert!(fixture.worktree().is_dir());
    assert!(fixture.branch_exists());
    assert!(report.contains(&fixture.reservation.branch), "{report}");
    assert!(
        report.contains(r#""reason":"uncollected_commits""#),
        "{report}"
    );
}

#[test]
fn reservation_with_dirty_changes_keeps_worktree_and_reports_reason() {
    let fixture = Fixture::new("fanout-reap-dirty");
    fs::write(fixture.worktree().join("dirty.txt"), "dirty\n").expect("write dirty work");
    fixture.age_past_gc_window();

    let output = fixture.run_gc();
    let report = String::from_utf8_lossy(&output.stderr);

    assert!(fixture.worktree().is_dir());
    assert!(fixture.branch_exists());
    assert!(report.contains(&fixture.reservation.branch), "{report}");
    assert!(
        report.contains(r#""reason":"worktree_not_clean""#),
        "{report}"
    );
}

#[test]
fn live_worker_reservation_is_untouched() {
    let fixture = Fixture::new("fanout-reap-live");
    let runtime_session_id = "22222222-2222-4222-8222-222222222222";
    let worker_id = activate_worker(
        &fixture.store,
        &fixture.reservation.reservation_id,
        runtime_session_id,
        fixture.worktree(),
    );
    let lifecycle = LifecycleStore::new(fixture.home.clone());
    let worker = lifecycle
        .read_worker(&worker_id)
        .expect("read live worker")
        .expect("live worker exists");
    let _custody = lifecycle
        .begin_owned_process_custody(
            &worker_id,
            &worker.generation,
            &store::uuid_v4(),
            ProcessObservation {
                pid: std::process::id(),
                pgid: None,
                start: StartGeneration::Unavailable,
            },
        )
        .expect("publish live process custody");
    fixture.age_past_gc_window();

    let output = fixture.run_gc();
    let report = String::from_utf8_lossy(&output.stderr);

    assert!(fixture.worktree().is_dir());
    assert!(fixture.branch_exists());
    assert!(!report.contains(&fixture.reservation.branch), "{report}");
}

#[test]
fn lost_authority_without_observation_is_untouched() {
    let fixture = Fixture::new("fanout-reap-lost-authority");
    let runtime_session_id = "33333333-3333-4333-8333-333333333333";
    let worker_id = activate_worker(
        &fixture.store,
        &fixture.reservation.reservation_id,
        runtime_session_id,
        fixture.worktree(),
    );
    let lifecycle = LifecycleStore::new(fixture.home.clone());
    let worker = lifecycle
        .read_worker(&worker_id)
        .expect("read worker")
        .expect("worker exists");
    let _custody = lifecycle
        .begin_owned_process_custody(
            &worker_id,
            &worker.generation,
            &store::uuid_v4(),
            ProcessObservation {
                pid: std::process::id(),
                pgid: None,
                start: StartGeneration::Unavailable,
            },
        )
        .expect("publish process custody");
    set_only_operation_to_lost_authority(&fixture.home);
    fixture.age_past_gc_window();

    let output = fixture.run_gc();
    let report = String::from_utf8_lossy(&output.stderr);

    assert!(fixture.worktree().is_dir());
    assert!(fixture.branch_exists());
    assert!(!report.contains(&fixture.reservation.branch), "{report}");
}

#[test]
fn managed_workspace_gate_refusal_skips_only_that_reservation() {
    let fixture = Fixture::new("fanout-reap-managed-workspace");
    fixture.age_past_gc_window();
    let opened = OpenedRepository::open(&fixture.repo).expect("open repository");
    let roots = SystemAuthorityRootProvider
        .roots()
        .expect("resolve authority roots");
    let authority = WorkspaceAuthority::new(roots).expect("open workspace authority");
    let repository_authority = authority
        .repository_dir(&opened.identity.repository_id)
        .expect("resolve repository authority");
    let _cleanup = RemoveDirOnDrop(repository_authority.clone());
    fs::create_dir(&repository_authority).expect("mark repository managed");

    let output = fixture.run_gc();
    let report = String::from_utf8_lossy(&output.stderr);

    assert!(fixture.worktree().is_dir());
    assert!(fixture.branch_exists());
    assert!(!report.contains(&fixture.reservation.branch), "{report}");
}

#[test]
fn legacy_flat_worktree_is_refused_and_reported() {
    let mut fixture = Fixture::new("fanout-reap-legacy-flat");
    let nested_worktree = fixture.worktree().to_path_buf();
    let legacy_worktree = fixture
        .home
        .join("worktrees")
        .join(&fixture.reservation.reservation_id);
    git(
        &fixture.repo,
        &[
            "worktree",
            "move",
            nested_worktree.to_str().expect("nested worktree is UTF-8"),
            legacy_worktree.to_str().expect("legacy worktree is UTF-8"),
        ],
    );
    mutate_fanout_record(
        &fixture.home,
        &fixture.reservation.reservation_id,
        |record| {
            record.insert(
                "worktree".into(),
                JsonValue::from(legacy_worktree.to_string_lossy().into_owned()),
            );
        },
    );
    fixture.reservation.worktree = legacy_worktree.to_string_lossy().into_owned();
    fixture.age_past_gc_window();

    let output = fixture.run_gc();
    let report = String::from_utf8_lossy(&output.stderr);
    let persisted = fixture
        .store
        .read(&fixture.reservation.reservation_id)
        .expect("read fanout authority")
        .expect("flat reservation remains");

    assert!(
        legacy_worktree.is_dir(),
        "legacy one-component worktree must be preserved"
    );
    assert!(fixture.branch_exists(), "legacy branch must be preserved");
    assert_eq!(
        persisted.worktree, fixture.reservation.worktree,
        "legacy persisted path must not be rewritten"
    );
    assert!(report.contains(&fixture.reservation.branch), "{report}");
    assert!(report.contains(r#""reason":"legacy_shape""#), "{report}");
}

#[test]
fn nested_worktree_past_fanout_cutoff_is_reaped() {
    let fixture = Fixture::new("fanout-reap-nested");
    let worktree = fixture.worktree().to_path_buf();
    let relative = worktree
        .strip_prefix(fixture.home.join("worktrees"))
        .expect("worktree is beneath worktrees root");
    let components = relative.components().collect::<Vec<_>>();
    assert_eq!(
        components.len(),
        2,
        "fixture must create repo-key/reservation-id"
    );
    assert_eq!(
        components.last().expect("reservation leaf").as_os_str(),
        fixture.reservation.reservation_id.as_str()
    );
    fixture.age_past_gc_window();

    fixture.run_gc();

    assert!(!worktree.exists(), "nested worktree was not reaped");
}

#[test]
fn two_day_fanout_worktree_is_reaped_while_shared_mailbox_is_retained() {
    let fixture = Fixture::new("fanout-reap-split-cutoff");
    let shared_mailbox = fixture
        .home
        .join("mailbox")
        .join("44444444-4444-4444-8444-444444444444.jsonl");
    fs::write(&shared_mailbox, "{\"body\":\"retain\"}\n").expect("write shared mailbox");
    let old = SystemTime::now() - Duration::from_secs((GC_DAYS + 1) * 24 * 60 * 60);
    let times = FileTimes::new().set_accessed(old).set_modified(old);
    fs::File::open(&shared_mailbox)
        .expect("open shared mailbox")
        .set_times(times)
        .expect("backdate shared mailbox");
    fixture.age_past_gc_window();

    fixture.run_gc_with_default_retention();

    assert!(
        !fixture.worktree().exists(),
        "two-day-old fanout worktree must cross the one-day cutoff"
    );
    assert!(
        shared_mailbox.is_file(),
        "two-day-old shared mailbox must remain inside the fourteen-day window"
    );
}
