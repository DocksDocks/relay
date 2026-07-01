// relay — session-relay's single binary. Subcommands land step by step per the
// rust-port plan: `bus`, `hook`, and the CLI verbs arrive in later steps; this
// step ships the store plus the hidden stress entry the cross-process lock
// test drives.

use std::collections::HashMap;
use tinyjson::JsonValue;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        // __stress <recipient-id> <who> <k> — mirrors test/selftest.mjs's
        // stress worker: race k enqueues against k register upserts, plus one
        // unique-id register per iteration so a lost read-modify-write shows
        // up as a missing registry entry.
        Some("__stress") => {
            if args.len() != 4 {
                die("usage: relay __stress <recipient-id> <who> <k>");
            }
            let (recipient, who) = (&args[1], &args[2]);
            let k: usize = args[3].parse().unwrap_or_else(|_| {
                die("k must be a number");
            });
            for i in 0..k {
                let mut msg: HashMap<String, JsonValue> = HashMap::new();
                msg.insert("from".into(), JsonValue::from(who.clone()));
                msg.insert("body".into(), JsonValue::from(format!("{who}-{i}")));
                relay::store::enqueue(recipient, &msg).unwrap_or_else(|e| die(&e));
                relay::store::register(who, Some(&format!("/tmp/{who}")), Some(who), None)
                    .unwrap_or_else(|e| die(&e));
                relay::store::register(&format!("{who}-op{i}"), Some("/tmp/x"), None, None)
                    .unwrap_or_else(|e| die(&e));
            }
        }
        _ => {
            die(
                "relay: available now: __stress (test helper). bus/hook/CLI subcommands land in later rust-port steps.",
            );
        }
    }
}

fn die(msg: &str) -> ! {
    eprintln!("{msg}");
    std::process::exit(1);
}
