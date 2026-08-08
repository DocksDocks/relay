// Cross-process proof that the flock upgrade preserves the store's multi-writer
// safety, plus the v1 mkdir-mutex migration. Everything runs through spawned
// `relay` child processes (env!("CARGO_BIN_EXE_relay")) with AGENT_RELAY_HOME
// set per child — no in-process env mutation, so tests can run in parallel.

pub mod support;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::process::Command;
use support::fresh_home;
use tinyjson::JsonValue;

fn obj(v: &JsonValue) -> &HashMap<String, JsonValue> {
    v.get::<HashMap<String, JsonValue>>().expect("object")
}

#[test]
fn concurrent_writers_no_lost_or_torn_writes() {
    let home = fresh_home("race");
    let bin = env!("CARGO_BIN_EXE_relay");
    let recipient = "dddddddd-dddd-dddd-dddd-dddddddddddd";
    let (n, k) = (8usize, 10usize);

    let children: Vec<_> = (0..n)
        .map(|w| {
            Command::new(bin)
                .args(["__stress", recipient, &format!("w{w}"), &k.to_string()])
                .env("AGENT_RELAY_HOME", &home)
                .spawn()
                .expect("spawn stress worker")
        })
        .collect();
    for mut c in children {
        assert!(c.wait().expect("wait").success(), "stress worker failed");
    }

    // Every enqueued line survives, none torn (mirrors selftest.mjs's check).
    let mail = fs::read_to_string(home.join("mailbox").join(format!("{recipient}.jsonl")))
        .expect("mailbox exists");
    let lines: Vec<&str> = mail.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), n * k, "lost or duplicated mailbox lines");
    let mut bodies = HashSet::new();
    for l in &lines {
        let v: JsonValue = l.parse().expect("torn JSONL line");
        let body = obj(&v)["body"].get::<String>().expect("body").clone();
        bodies.insert(body);
        assert!(obj(&v).contains_key("id") && obj(&v).contains_key("ts"));
    }
    assert_eq!(bodies.len(), n * k, "duplicate bodies — a write was lost");

    // Registry read-modify-write under contention: 8 worker ids + 80 unique
    // per-op ids must ALL be present — a lost RMW shows up as a missing entry.
    let reg: JsonValue = fs::read_to_string(home.join("registry.json"))
        .expect("registry exists")
        .parse()
        .expect("registry parses");
    let agents = obj(&obj(&reg)["agents"]);
    assert_eq!(
        agents.len(),
        n + n * k,
        "lost registry upsert under contention"
    );
    let names = obj(&obj(&reg)["names"]);
    assert_eq!(
        names.len(),
        n,
        "names index should hold exactly the 8 workers"
    );

    // The lock is a FILE now (flock), not the v1 mkdir-mutex directory.
    assert!(
        fs::metadata(home.join(".lock"))
            .expect(".lock exists")
            .is_file()
    );

    fs::remove_dir_all(&home).ok();
}

#[test]
fn legacy_lock_dir_is_migrated_to_a_file() {
    let home = fresh_home("migrate");
    // Simulate an abandoned v1 mkdir-mutex: `.lock` exists as a DIRECTORY.
    fs::create_dir(home.join(".lock")).unwrap();

    let bin = env!("CARGO_BIN_EXE_relay");
    let status = Command::new(bin)
        .args([
            "__stress",
            "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
            "mig",
            "1",
        ])
        .env("AGENT_RELAY_HOME", &home)
        .status()
        .expect("spawn");
    assert!(
        status.success(),
        "store op must succeed over a stale .lock dir"
    );

    assert!(
        fs::metadata(home.join(".lock"))
            .expect(".lock exists")
            .is_file(),
        ".lock dir was not migrated to a flock file"
    );
    let mail = fs::read_to_string(
        home.join("mailbox")
            .join("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa.jsonl"),
    )
    .expect("mailbox written after migration");
    assert_eq!(mail.lines().count(), 1);

    fs::remove_dir_all(&home).ok();
}
