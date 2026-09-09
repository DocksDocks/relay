#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration test crate: a panic is the intended failure signal"
)]

pub mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use support::fresh_home;

const VALID: &str = "01a081c6-19da-737a-a863-9fb9d50ad5c2";
const INVALID: &str = "--config=evil";
const UPPER: &str = "01A081C6-19DA-737A-A863-9FB9D50AD5C2";

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
/// whose id is invalid beside one valid session, both with queued mail.
fn seed_tampered_registry(home: &Path, invalid: &str) {
    let dir = home.display();
    let entry = |id: &str, name: &str| {
        format!(
            "\"{id}\":{{\"id\":\"{id}\",\"dir\":\"{dir}\",\"name\":\"{name}\",\"tool\":\"omp\",\"lastSeen\":\"2026-09-09T00:00:00.000Z\"}}"
        )
    };
    fs::write(
        home.join("registry.json"),
        format!(
            "{{\"agents\":{{{},{}}},\"names\":{{\"bad\":\"{invalid}\",\"good\":\"{VALID}\"}}}}",
            entry(invalid, "bad"),
            entry(VALID, "good")
        ),
    )
    .unwrap();
    fs::create_dir_all(home.join("mailbox")).unwrap();
    for id in [invalid, VALID] {
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
    seed_tampered_registry(&home, INVALID);

    let named = relay(&home, &launcher, &["watch", "bad", "--once"]);
    assert!(!named.status.success(), "watch bad must refuse");
    assert!(
        String::from_utf8_lossy(&named.stderr).contains("unknown session"),
        "watch bad must name the refusal: {}",
        String::from_utf8_lossy(&named.stderr)
    );
    assert!(!log.exists(), "named watch launched the invalid id");

    let all = relay(&home, &launcher, &["watch", "--all", "--once"]);
    assert!(
        all.status.success(),
        "watch --all must skip the bad entry without failing: {}",
        String::from_utf8_lossy(&all.stderr)
    );
    assert!(
        !String::from_utf8_lossy(&all.stderr).contains(INVALID),
        "watch --all must not see the bad entry: {}",
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

#[test]
fn register_refuses_an_uppercase_session_id() {
    let home = fresh_home("watch-register-refuses-uppercase");
    let (launcher, _log) = recording_launcher(&home);
    let dir = home.to_string_lossy().into_owned();
    let output = relay(
        &home,
        &launcher,
        &["register", "bad", "--id", UPPER, "--dir", &dir],
    );
    assert!(!output.status.success(), "register must refuse {UPPER}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("session UUID (lowercase)"),
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
fn watch_refuses_and_skips_an_uppercase_session_id() {
    let home = fresh_home("watch-uppercase-targets");
    let (launcher, log) = recording_launcher(&home);
    seed_tampered_registry(&home, UPPER);

    let named = relay(&home, &launcher, &["watch", UPPER, "--once"]);
    assert!(!named.status.success(), "watch {UPPER} must refuse");
    assert!(
        String::from_utf8_lossy(&named.stderr).contains("unknown session"),
        "watch must name the refusal: {}",
        String::from_utf8_lossy(&named.stderr)
    );
    assert!(!log.exists(), "named watch launched the uppercase id");

    // `--once` beside `--follow` is rejected after the id gate; on a regression the
    // combination check exits without blocking and the message assertion fails.
    for flag in ["--id", "--follow"] {
        let flagged = relay(&home, &launcher, &["watch", flag, UPPER, "--once"]);
        assert!(
            !flagged.status.success(),
            "watch {flag} {UPPER} must refuse"
        );
        assert!(
            String::from_utf8_lossy(&flagged.stderr).contains("session UUID (lowercase)"),
            "watch {flag} must name the refusal: {}",
            String::from_utf8_lossy(&flagged.stderr)
        );
        assert!(!log.exists(), "watch {flag} launched the uppercase id");
        assert!(
            !home.join("watchers").exists(),
            "watch {flag} must not create a watcher lock"
        );
    }

    let all = relay(&home, &launcher, &["watch", "--all", "--once"]);
    assert!(
        all.status.success(),
        "watch --all must skip the uppercase entry without failing: {}",
        String::from_utf8_lossy(&all.stderr)
    );
    assert!(
        !String::from_utf8_lossy(&all.stderr).contains(UPPER),
        "watch --all must not see the uppercase entry: {}",
        String::from_utf8_lossy(&all.stderr)
    );
    let launched = fs::read_to_string(&log).unwrap_or_default();
    assert!(
        launched.contains(&format!("--resume {VALID}")),
        "watch --all must still wake the valid session: {launched:?}"
    );
    assert!(
        !launched.contains(UPPER),
        "watch --all launched the uppercase id: {launched:?}"
    );
    fs::remove_dir_all(home).ok();
}

#[test]
fn readers_ignore_an_uppercase_registry_entry() {
    let home = fresh_home("readers-uppercase");
    let (launcher, log) = recording_launcher(&home);
    seed_tampered_registry(&home, UPPER);

    let list = relay(&home, &launcher, &["list"]);
    assert!(list.status.success(), "list must not fail on a bad entry");
    let listed = String::from_utf8_lossy(&list.stdout);
    assert!(
        listed.contains(VALID),
        "list must keep the valid entry: {listed}"
    );
    assert!(
        !listed.contains(UPPER),
        "list must drop the uppercase entry: {listed}"
    );

    let attach = relay(&home, &launcher, &["attach", UPPER]);
    assert!(!attach.status.success(), "attach {UPPER} must refuse");
    assert!(
        String::from_utf8_lossy(&attach.stderr).contains("non-session UUID (lowercase)"),
        "attach must name the refusal: {}",
        String::from_utf8_lossy(&attach.stderr)
    );
    assert!(!log.exists(), "attach launched the uppercase id");

    let doctor = relay(&home, &launcher, &["doctor", "--id", UPPER]);
    assert_eq!(doctor.status.code(), Some(1), "doctor {UPPER} must exit 1");
    let report = String::from_utf8_lossy(&doctor.stdout);
    assert!(
        report.contains("FAIL identity: unknown session"),
        "doctor must report the unknown session: {report}"
    );

    let dir = home.to_string_lossy().into_owned();
    let wake = relay(
        &home,
        &launcher,
        &["wake", "--id", UPPER, "--dir", &dir, "--dry"],
    );
    assert!(!wake.status.success(), "wake --id {UPPER} must refuse");
    assert!(
        String::from_utf8_lossy(&wake.stderr).contains("--id must be a session UUID (lowercase)"),
        "wake must name the refusal: {}",
        String::from_utf8_lossy(&wake.stderr)
    );
    assert!(!log.exists(), "wake launched the uppercase id");
    fs::remove_dir_all(home).ok();
}
