use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub fn fresh_home(tag: &str) -> PathBuf {
    let home = std::env::temp_dir().join(format!(
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
