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
