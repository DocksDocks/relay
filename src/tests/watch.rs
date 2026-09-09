pub mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use support::fresh_home;

const VALID: &str = "01a081c6-19da-737a-a863-9fb9d50ad5c2";
const INVALID: &str = "--config=evil";

/// A recording launcher: every argv it receives is appended to `argv.log`.
fn recording_launcher(home: &Path) -> (String, std::path::PathBuf) {
    let log = home.join("argv.log");
    let launcher = home.join("fake-omp");
    fs::write(
        &launcher,
        format!("#!/bin/sh\necho \"$*\" >> {}\nexit 0\n", log.display()),
    )
    .unwrap();
    fs::set_permissions(&launcher, fs::Permissions::from_mode(0o700)).unwrap();
    (launcher.to_string_lossy().into_owned(), log)
}

fn relay(home: &Path, launcher: &str, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_relay"))
        .args(args)
        .env("AGENT_RELAY_HOME", home)
        .env("RELAY_WAKE_CMD_OMP", launcher)
        .output()
        .unwrap()
}

/// Write a registry as a tampered or pre-validation writer would: one entry
/// whose id is flag-shaped beside one valid session, both with queued mail.
fn seed_tampered_registry(home: &Path) {
    let dir = home.display();
    let entry = |id: &str, name: &str| {
        format!(
            "\"{id}\":{{\"id\":\"{id}\",\"dir\":\"{dir}\",\"name\":\"{name}\",\"tool\":\"omp\",\"lastSeen\":\"2026-09-09T00:00:00.000Z\"}}"
        )
    };
    fs::write(
        home.join("registry.json"),
        format!(
            "{{\"agents\":{{{},{}}},\"names\":{{\"bad\":\"{INVALID}\",\"good\":\"{VALID}\"}}}}",
            entry(INVALID, "bad"),
            entry(VALID, "good")
        ),
    )
    .unwrap();
    fs::create_dir_all(home.join("mailbox")).unwrap();
    for id in [INVALID, VALID] {
        fs::write(
            home.join("mailbox")
                .join(format!("{}.jsonl", relay::store::sanitize(id))),
            "{\"id\":\"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa\",\"from\":\"x\",\"body\":\"mail\",\"ts\":\"2026-09-09T00:00:00.000Z\"}\n",
        )
        .unwrap();
    }
}

#[test]
fn register_refuses_a_non_uuid_session_id() {
    let home = fresh_home("watch-register-refuses-non-uuid");
    let (launcher, _log) = recording_launcher(&home);
    let dir = home.to_string_lossy().into_owned();
    let output = relay(
        &home,
        &launcher,
        &[
            "register", "bad", "--id", INVALID, "--dir", &dir, "--tool", "omp",
        ],
    );
    assert!(!output.status.success(), "register must refuse {INVALID}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("session UUID"),
        "register must name the refusal: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !home.join("registry.json").exists(),
        "refusal must not write"
    );
    fs::remove_dir_all(home).ok();
}

#[test]
fn watch_never_launches_a_tampered_non_uuid_target() {
    let home = fresh_home("watch-non-uuid-targets");
    let (launcher, log) = recording_launcher(&home);
    seed_tampered_registry(&home);

    let named = relay(&home, &launcher, &["watch", "bad", "--once"]);
    assert!(!named.status.success(), "watch bad must refuse");
    assert!(
        String::from_utf8_lossy(&named.stderr).contains("not a session UUID"),
        "watch bad must name the refusal"
    );
    assert!(!log.exists(), "named watch launched the invalid id");

    let all = relay(&home, &launcher, &["watch", "--all", "--once"]);
    assert!(
        all.status.success(),
        "watch --all must skip the bad entry without failing: {}",
        String::from_utf8_lossy(&all.stderr)
    );
    assert!(
        String::from_utf8_lossy(&all.stderr).contains("skip --config=evil"),
        "watch --all must report the skipped id: {}",
        String::from_utf8_lossy(&all.stderr)
    );
    let launched = fs::read_to_string(&log).unwrap_or_default();
    assert!(
        launched.contains(&format!("--resume {VALID}")),
        "watch --all must still wake the valid session: {launched:?}"
    );
    assert!(
        !launched.contains(INVALID),
        "watch --all launched the invalid id: {launched:?}"
    );
    fs::remove_dir_all(home).ok();
}
