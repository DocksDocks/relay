// Black-box self-replacement tests: drive the Unix rename path and the staged
// identity check of `relay update` against real files on disk.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration test crate: a panic is the intended failure signal"
)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A scratch directory of its own per case, so cases stay independent when the
/// harness runs them on separate threads.
fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("relay-update-{}-{tag}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// A staged "binary" that only has to answer `--version`.
fn stage_script(path: &Path, version_line: &str) {
    fs::write(path, format!("#!/bin/sh\necho '{version_line}'\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn version_output(binary: &Path) -> String {
    let output = Command::new(binary)
        .arg("--version")
        .output()
        .expect("run the replaced binary");
    assert!(output.status.success(), "--version failed: {output:?}");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

#[test]
fn replace_executable_swaps_the_target_in_place() {
    let dir = scratch("swap");
    let target = dir.join("relay");
    fs::copy(env!("CARGO_BIN_EXE_relay"), &target).unwrap();
    assert_eq!(
        version_output(&target),
        format!("relay {}", env!("CARGO_PKG_VERSION"))
    );

    let staged = dir.join("relay.update-1");
    stage_script(&staged, "relay 0.0.0-staged");
    relay::update::replace_executable(&target, &staged).unwrap();

    assert_eq!(version_output(&target), "relay 0.0.0-staged");
    assert!(!staged.exists(), "staged file survived the rename");
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn replace_executable_refuses_an_unwritable_directory() {
    if rustix::process::geteuid().is_root() {
        println!("skipped: root ignores directory mode bits");
        return;
    }
    let dir = scratch("unwritable");
    let target = dir.join("relay");
    fs::copy(env!("CARGO_BIN_EXE_relay"), &target).unwrap();
    let original = fs::read(&target).unwrap();
    let staged = dir.join("relay.update-2");
    stage_script(&staged, "relay 0.0.0-staged");

    fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();
    let outcome = relay::update::replace_executable(&target, &staged);
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();

    let error = outcome.expect_err("rename into a read-only directory must fail");
    assert!(error.contains("not writable"), "unexpected error: {error}");
    assert_eq!(fs::read(&target).unwrap(), original, "target was modified");
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn verify_staged_removes_a_mismatching_binary() {
    let dir = scratch("verify");
    let staged = dir.join("relay.update-3");

    stage_script(&staged, "relay 9.9.9");
    let error = relay::update::verify_staged(&staged, "v0.19.0")
        .expect_err("a foreign version must be rejected");
    assert!(
        error.contains("staged binary reports relay 9.9.9"),
        "unexpected error: {error}"
    );
    assert!(!staged.exists(), "mismatching staged file survived");

    stage_script(&staged, "relay 0.19.0");
    relay::update::verify_staged(&staged, "v0.19.0").unwrap();
    assert!(staged.exists(), "matching staged file was removed");
    fs::remove_dir_all(&dir).unwrap();
}
