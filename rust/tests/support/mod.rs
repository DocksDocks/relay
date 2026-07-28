pub mod fanout;
pub mod workspace;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Once;
use std::time::{Duration, SystemTime};

/// Homes abandoned for a full day cannot belong to a live test binary.
const STALE_HOME_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// One sweep per test binary. `fresh_home` has 54 call sites, and the tree this
/// scans can hold hundreds of entries, so sweeping per call would pay a full
/// readdir every time for no additional benefit.
static SWEEP_ONCE: Once = Once::new();

/// Reclaim `relay-test-*` homes left by earlier runs.
///
/// `fresh_home` hands back a plain `PathBuf` with no RAII guard, and cleanup is
/// left to each caller: many never do it, and a panicking test never reaches
/// its `remove_dir_all`. So homes accumulate even across fully passing runs -
/// one observed tree held 517 of them from 76 distinct PIDs. Sweeping on entry
/// keeps that bounded without changing the signature at 54 call sites, and
/// preserves same-day directories for post-mortem inspection. Only entries
/// older than a full day are removed, so a concurrently running test binary's
/// homes are never touched.
fn sweep_stale_homes(root: &Path) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        if !entry
            .file_name()
            .to_string_lossy()
            .starts_with("relay-test-")
        {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age >= STALE_HOME_AGE);
        if stale {
            fs::remove_dir_all(entry.path()).ok();
        }
    }
}

pub fn fresh_home(tag: &str) -> PathBuf {
    let root = std::env::var_os("SESSION_RELAY_TEST_WORKSPACE_ROOT")
        .map(PathBuf::from)
        .or_else(|| option_env!("CARGO_TARGET_TMPDIR").map(PathBuf::from))
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

pub fn write_executable(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// Test-only owner for a detached supervisor's process group.
///
/// Supervisors are put in their own session, so a managed child inherits the
/// supervisor PID as its PGID. Tests that destroy supervisor or watchdog
/// authority on purpose leave that group with no owner: the shipped lifecycle
/// records `LostAuthority` rather than falsely claiming a reap, which is
/// correct production behaviour and must not change to satisfy a fixture.
///
/// Without an independent owner the managed child outlives the test binary.
/// The stubs those tests install ignore TERM/HUP/INT and loop forever, so they
/// accumulate across runs - one observed host carried stubs 16 hours old from
/// several distinct runs, adding load to every later test. `sweep_stale_homes`
/// cannot help: it reclaims directories, and these are processes.
///
/// Arm this as soon as the supervisor PID is known; `Drop` covers the
/// panicking and early-return paths that never reach an explicit cleanup.
pub struct SupervisorGroupReaper {
    pgid: Option<libc::pid_t>,
}

impl SupervisorGroupReaper {
    /// Arm only if `pgid` is genuinely its own group leader.
    ///
    /// Leadership is checked here, while the process is known live, rather
    /// than at reap time: once the leader is SIGKILLed and reaped, `getpgid`
    /// returns `ESRCH` even though the group still holds the orphan we came
    /// for, so a teardown-time check would skip exactly the case that matters.
    /// Proving it now also rules out signalling a recycled PID's group later.
    /// A non-leader means `setsid` never took and `-pid` was never ours to
    /// signal, so the reaper stays inert.
    pub fn arm(pgid: libc::pid_t) -> Self {
        let leads = pgid > 1 && unsafe { libc::getpgid(pgid) } == pgid;
        if !leads {
            eprintln!(
                "SupervisorGroupReaper: pid {pgid} is not a process-group leader, so no \
                 orphan cleanup will run. Arm only after the supervisor is observed Ready; \
                 arming between fork and the child's setsid still reports our own group."
            );
        }
        Self {
            pgid: leads.then_some(pgid),
        }
    }

    /// SIGKILL the group, guarding the ways `kill(-pgid, ...)` goes wrong.
    ///
    /// Signalling our own group would SIGKILL the test runner itself, which
    /// presents as a vanished harness rather than a test failure. Probing with
    /// signal 0 first distinguishes "already gone" from a real delivery.
    pub fn reap(&mut self) {
        let Some(pgid) = self.pgid.take() else {
            return;
        };
        if pgid == unsafe { libc::getpgrp() } {
            return;
        }
        if unsafe { libc::kill(-pgid, 0) } != 0
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        {
            return;
        }
        unsafe { libc::kill(-pgid, libc::SIGKILL) };
    }
}

impl Drop for SupervisorGroupReaper {
    fn drop(&mut self) {
        self.reap();
    }
}
