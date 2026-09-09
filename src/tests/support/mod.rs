use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Once;
use std::time::{Duration, SystemTime};

/// Age fallback for homes without an identifiable owner process.
const STALE_HOME_AGE: Duration = Duration::from_secs(60 * 60);

/// A dead PID can be recycled onto a fresh test binary, which would then be
/// holding a home this sweep is about to delete. Requiring the home to also be
/// untouched for this long makes that window unreachable in practice: the
/// recycled process would have to create its home and then leave it idle for
/// five minutes before the sweep looked at it.
const PID_REUSE_GRACE: Duration = Duration::from_secs(5 * 60);

/// Sweep once per test binary rather than once per fixture.
static SWEEP_ONCE: Once = Once::new();

/// Reclaim abandoned homes without deleting live fixtures or recently reused PIDs.
fn sweep_stale_homes(root: &Path) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    let now = SystemTime::now();
    let self_pid = std::process::id();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("relay-test-") {
            continue;
        }
        let Ok(idle) = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .map(|modified| now.duration_since(modified).unwrap_or_default())
        else {
            continue;
        };
        let stale = match home_owner_pid(&name) {
            Some(pid) => pid != self_pid && !pid_is_alive(pid) && idle >= PID_REUSE_GRACE,
            None => idle >= STALE_HOME_AGE,
        };
        if stale {
            fs::remove_dir_all(entry.path()).ok();
        }
    }
}

/// Recover the PID `fresh_home` stamped into `relay-test-{tag}-{pid}-{uuid}`.
///
/// The tag itself contains hyphens ("repository-gate"), so the PID can only be
/// found from the right: the UUID is always the last five hyphen-separated
/// fields.
fn home_owner_pid(name: &str) -> Option<u32> {
    let rest = name.strip_prefix("relay-test-")?;
    let mut fields = rest.rsplit('-').skip(5);
    let pid: u32 = fields.next()?.parse().ok()?;
    // A tag must precede the PID; a bare `relay-test-<pid>-<uuid>` is not ours.
    fields.next()?;
    (pid > 0).then_some(pid)
}

/// `EPERM` means the PID exists and belongs to someone else, which is still
/// alive for our purposes. Only `ESRCH` proves nobody holds it.
fn pid_is_alive(pid: u32) -> bool {
    if unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

pub fn fresh_home(tag: &str) -> PathBuf {
    let root = option_env!("CARGO_TARGET_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let home = root.join(format!(
        "relay-test-{tag}-{}-{}",
        std::process::id(),
        relay::store::uuid_v4()
    ));
    SWEEP_ONCE.call_once(|| sweep_stale_homes(&root));
    fs::create_dir_all(&home).unwrap();
    home
}

/// Run root-sensitive store tests in a fresh process without mutating the
/// environment of concurrent test threads.
pub fn isolated_home(test_name: &str) -> Option<PathBuf> {
    if std::env::var("RELAY_TEST_ISOLATED_CASE").as_deref() == Ok(test_name) {
        return Some(PathBuf::from(std::env::var_os("AGENT_RELAY_HOME").unwrap()));
    }
    let home = fresh_home(test_name);
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
        .env("RELAY_TEST_ISOLATED_CASE", test_name)
        .env("AGENT_RELAY_HOME", &home)
        .output()
        .expect("spawn isolated store test");
    fs::remove_dir_all(&home).ok();
    assert!(
        output.status.success(),
        "isolated {test_name} failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    None
}
