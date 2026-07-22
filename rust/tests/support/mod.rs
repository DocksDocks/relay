pub mod fanout;
pub mod workspace;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

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
    fs::create_dir_all(&home).unwrap();
    home
}

pub fn write_executable(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}
