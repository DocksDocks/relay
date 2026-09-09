#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration test crate: a panic is the intended failure signal"
)]

pub mod support;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use support::isolated_home;
use tinyjson::JsonValue;

const SESSION: &str = "91111111-1111-4111-8111-111111111111";

fn seed_mailbox(home: &Path) -> PathBuf {
    fs::create_dir_all(home.join("mailbox")).unwrap();
    let mailbox = home.join("mailbox").join(format!("{SESSION}.jsonl"));
    fs::write(&mailbox, b"{\"body\":\"held\"}\n").unwrap();
    mailbox
}

#[test]
fn omp_drain_can_restore_mail_then_consume_it_once() {
    let Some(home) = isolated_home("omp_drain_can_restore_mail_then_consume_it_once") else {
        return;
    };
    let session = SESSION;
    seed_mailbox(&home);
    let mailbox = home.join("mailbox").join(format!("{session}.jsonl"));
    let original = "{\"id\":\"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa\",\"body\":\"omp mail\"}\n";
    fs::write(&mailbox, original).unwrap();

    let receipt = relay::store::drain_mailbox(session).unwrap();
    assert!(!mailbox.exists());
    receipt.rollback().unwrap();
    assert_eq!(fs::read_to_string(&mailbox).unwrap(), original);
    assert_eq!(
        relay::store::drain_mailbox(session)
            .unwrap()
            .into_messages()
            .len(),
        1
    );
    assert!(
        relay::store::drain_mailbox(session)
            .unwrap()
            .into_messages()
            .is_empty()
    );
    fs::remove_dir_all(home).ok();
}

#[test]
fn drain_receipt_rolls_back_exact_lines_before_new_mail() {
    let Some(home) = isolated_home("drain_receipt_rolls_back_exact_lines_before_new_mail") else {
        return;
    };
    let session = SESSION;
    seed_mailbox(&home);
    let mailbox = home.join("mailbox").join(format!("{session}.jsonl"));
    let original = "{\"id\":\"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa\",\"body\":\"first\"}\n{\"id\":\"bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb\",\"body\":\"second\"}\n";
    fs::write(&mailbox, original).unwrap();

    let receipt = relay::store::drain_mailbox(session).unwrap();
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&mailbox)
        .unwrap()
        .write_all(b"{\"id\":\"cccccccc-cccc-4ccc-8ccc-cccccccccccc\",\"body\":\"third\"}\n")
        .unwrap();
    receipt.rollback().unwrap();
    let restored = fs::read_to_string(&mailbox).unwrap();
    assert!(restored.starts_with(original));
    assert!(restored.ends_with('\n'));
    assert_eq!(
        restored
            .matches("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa")
            .count(),
        1
    );
    assert_eq!(
        restored
            .matches("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb")
            .count(),
        1
    );
    assert!(restored.rfind("third").unwrap() > restored.find("second").unwrap());
    fs::remove_dir_all(home).ok();
}

#[test]
fn hold_ack_consumes_only_held_mail_and_rejects_reused_token() {
    let Some(home) = isolated_home("hold_ack_consumes_only_held_mail_and_rejects_reused_token")
    else {
        return;
    };
    let mailbox = seed_mailbox(&home);
    let receipt = relay::store::hold_mailbox(SESSION, "inbox", 30).unwrap();
    assert_eq!(receipt.count, 1);
    assert_eq!(
        receipt.messages,
        vec![r#"{"body":"held"}"#.parse::<JsonValue>().unwrap()]
    );
    assert_eq!(receipt.raw, b"{\"body\":\"held\"}\n");
    fs::write(&mailbox, b"{\"body\":\"later\"}\n").unwrap();
    let holds = relay::store::HoldStore::new(home.clone());
    let token = receipt.token.as_deref().unwrap();
    holds.ack_hold(token).unwrap();
    assert_eq!(holds.ack_hold(token), Err(relay::store::HoldError::Unknown));
    assert_eq!(
        relay::store::drain_mailbox(SESSION)
            .unwrap()
            .into_messages(),
        vec![r#"{"body":"later"}"#.parse::<JsonValue>().unwrap()]
    );
    assert!(
        relay::store::drain_mailbox(SESSION)
            .unwrap()
            .into_messages()
            .is_empty()
    );

    fs::remove_dir_all(home).ok();
}

#[test]
fn hold_rollback_restores_exact_bytes_before_later_mail_once() {
    let Some(home) = isolated_home("hold_rollback_restores_exact_bytes_before_later_mail_once")
    else {
        return;
    };
    let mailbox = seed_mailbox(&home);
    let original = b"{\"body\":\"held\"}\nmalformed\n\n";
    fs::write(&mailbox, original).unwrap();
    let receipt = relay::store::hold_mailbox(SESSION, "inbox", 30).unwrap();
    fs::write(&mailbox, b"{\"body\":\"later\"}\n").unwrap();
    let holds = relay::store::HoldStore::new(home.clone());
    let token = receipt.token.as_deref().unwrap();
    holds.rollback_hold(token).unwrap();
    assert_eq!(
        fs::read(&mailbox).unwrap(),
        [original.as_slice(), b"{\"body\":\"later\"}\n"].concat()
    );
    assert_eq!(
        holds.rollback_hold(token),
        Err(relay::store::HoldError::Unknown)
    );
    assert_eq!(
        relay::store::drain_mailbox(SESSION)
            .unwrap()
            .into_messages(),
        vec![
            r#"{"body":"held"}"#.parse::<JsonValue>().unwrap(),
            r#"{"body":"later"}"#.parse::<JsonValue>().unwrap(),
        ]
    );
    assert!(
        relay::store::drain_mailbox(SESSION)
            .unwrap()
            .into_messages()
            .is_empty()
    );

    fs::remove_dir_all(home).ok();
}

#[test]
fn hold_expiry_rejects_ack_and_restores_one_row() {
    let Some(home) = isolated_home("hold_expiry_rejects_ack_and_restores_one_row") else {
        return;
    };
    seed_mailbox(&home);
    let receipt = relay::store::hold_mailbox(SESSION, "inbox", 0).unwrap();
    assert_eq!(
        relay::store::HoldStore::new(home.clone()).ack_hold(receipt.token.as_deref().unwrap()),
        Err(relay::store::HoldError::Expired)
    );
    assert_eq!(
        relay::store::drain_mailbox(SESSION)
            .unwrap()
            .into_messages(),
        vec![r#"{"body":"held"}"#.parse::<JsonValue>().unwrap()]
    );
    assert!(
        relay::store::drain_mailbox(SESSION)
            .unwrap()
            .into_messages()
            .is_empty()
    );

    fs::remove_dir_all(home).ok();
}

#[test]
fn hold_conflict_preserves_first_hold_and_later_mail() {
    let Some(home) = isolated_home("hold_conflict_preserves_first_hold_and_later_mail") else {
        return;
    };
    let mailbox = seed_mailbox(&home);
    let receipt = relay::store::hold_mailbox(SESSION, "inbox", 30).unwrap();
    fs::write(&mailbox, b"{\"body\":\"later\"}\n").unwrap();
    let error = match relay::store::hold_mailbox(SESSION, "inbox", 30) {
        Ok(_) => panic!("second hold unexpectedly succeeded"),
        Err(error) => error,
    };
    assert_eq!(error, "hold_conflict");
    assert_eq!(fs::read(&mailbox).unwrap(), b"{\"body\":\"later\"}\n");
    relay::store::HoldStore::new(home.clone())
        .rollback_hold(receipt.token.as_deref().unwrap())
        .unwrap();
    assert_eq!(
        fs::read(&mailbox).unwrap(),
        b"{\"body\":\"held\"}\n{\"body\":\"later\"}\n"
    );

    fs::remove_dir_all(home).ok();
}

#[test]
fn hold_committing_recovery_never_restores_consumed_row() {
    let Some(home) = isolated_home("hold_committing_recovery_never_restores_consumed_row") else {
        return;
    };
    let mailbox = seed_mailbox(&home);
    let receipt = relay::store::hold_mailbox(SESSION, "inbox", 30).unwrap();
    fs::write(&mailbox, b"{\"body\":\"later\"}\n").unwrap();
    let broken = relay::store::HoldStore::new(home.clone())
        .with_failpoint(relay::store::HoldFailpoint::AfterPhaseWrite);
    assert!(broken.ack_hold(receipt.token.as_deref().unwrap()).is_err());
    assert_eq!(
        relay::store::drain_mailbox(SESSION)
            .unwrap()
            .into_messages(),
        vec![r#"{"body":"later"}"#.parse::<JsonValue>().unwrap()]
    );
    assert!(
        relay::store::drain_mailbox(SESSION)
            .unwrap()
            .into_messages()
            .is_empty()
    );

    fs::remove_dir_all(home).ok();
}

#[test]
fn hold_restoring_recovery_restores_exactly_one_row() {
    for point in [
        relay::store::HoldFailpoint::AfterPhaseWrite,
        relay::store::HoldFailpoint::AfterRestoreRename,
    ] {
        let Some(home) = isolated_home("hold_restoring_recovery_restores_exactly_one_row") else {
            return;
        };
        seed_mailbox(&home);
        let receipt = relay::store::hold_mailbox(SESSION, "inbox", 30).unwrap();
        let broken = relay::store::HoldStore::new(home.clone()).with_failpoint(point);
        assert!(
            broken
                .rollback_hold(receipt.token.as_deref().unwrap())
                .is_err()
        );
        assert_eq!(
            relay::store::drain_mailbox(SESSION)
                .unwrap()
                .into_messages(),
            vec![r#"{"body":"held"}"#.parse::<JsonValue>().unwrap()],
            "{point:?}"
        );
        assert!(
            relay::store::drain_mailbox(SESSION)
                .unwrap()
                .into_messages()
                .is_empty()
        );

        fs::remove_dir_all(home).ok();
    }
}

#[test]
fn hold_after_manifest_crash_state_recovers_exactly_one_row() {
    let Some(home) = isolated_home("hold_after_manifest_crash_state_recovers_exactly_one_row")
    else {
        return;
    };
    let mailbox = seed_mailbox(&home);
    let receipt = relay::store::hold_mailbox(SESSION, "inbox", 30).unwrap();
    let token = receipt.token.as_deref().unwrap();
    // Recreate the durable state before creation moves the mailbox into custody.
    fs::rename(home.join("holds").join(format!("{token}.jsonl")), &mailbox).unwrap();
    assert_eq!(
        relay::store::drain_mailbox(SESSION)
            .unwrap()
            .into_messages(),
        vec![r#"{"body":"held"}"#.parse::<JsonValue>().unwrap()]
    );
    assert!(
        relay::store::drain_mailbox(SESSION)
            .unwrap()
            .into_messages()
            .is_empty()
    );
    assert_eq!(
        relay::store::HoldStore::new(home.clone()).ack_hold(token),
        Err(relay::store::HoldError::Unknown)
    );

    fs::remove_dir_all(home).ok();
}

#[test]
fn hold_after_mailbox_rename_crash_state_expires_to_exactly_one_row() {
    let Some(home) =
        isolated_home("hold_after_mailbox_rename_crash_state_expires_to_exactly_one_row")
    else {
        return;
    };
    seed_mailbox(&home);
    // A lost receipt after the rename leaves the same durable state as a hold
    // whose caller exits without settling it. Zero seconds makes recovery deterministic.
    relay::store::hold_mailbox(SESSION, "inbox", 0).unwrap();
    assert_eq!(
        relay::store::drain_mailbox(SESSION)
            .unwrap()
            .into_messages(),
        vec![r#"{"body":"held"}"#.parse::<JsonValue>().unwrap()]
    );
    assert!(
        relay::store::drain_mailbox(SESSION)
            .unwrap()
            .into_messages()
            .is_empty()
    );

    fs::remove_dir_all(home).ok();
}
