// relay — durable messaging between omp sessions.

use std::collections::HashMap;
use tinyjson::JsonValue;

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match argv.first().map(String::as_str) {
        Some("--version") => println!("session-relay {}", env!("CARGO_PKG_VERSION")),
        Some("bus") => relay::bus::run(),
        Some("hook") => relay::hook::run(&argv[1..]),
        Some(
            cmd @ ("discover" | "list" | "register" | "send" | "request" | "reply" | "inbox"
            | "ack" | "rollback" | "peek" | "attach" | "wake" | "doctor"),
        ) => relay::cli::run(cmd, argv.clone()),
        Some("watch") => relay::watch::run(argv.clone()),
        // __stress <recipient-id> <who> <k> — mirrors test/selftest.mjs's
        // stress worker: race k enqueues against k register upserts, plus one
        // unique-id register per iteration so a lost read-modify-write shows
        // up as a missing registry entry.
        Some("__stress") => {
            if argv.len() != 4 {
                die("usage: relay __stress <recipient-id> <who> <k>");
            }
            let (recipient, who) = (&argv[1], &argv[2]);
            let k: usize = argv[3].parse().unwrap_or_else(|_| {
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
        _ => die(USAGE),
    }
}
const USAGE: &str = "usage: relay bus | hook omp --session <id> --cwd <dir> [--event prompt] [--hold [<seconds>]] | discover [--within min] [--tool omp] | list | register <name> --id <uuid> [--dir <path>] | send <to> [--] <msg> | request <to> [--from <registered>] [--json] [--] <msg> | reply <correlation-id> [--from <registered>] --status completed|failed [--] <msg> | inbox [--hold [<seconds>]] <who> | ack <token> | rollback <token> | peek <who> | attach <who> [--exec] | wake <who> [--effort e] [msg] | doctor [--id <session>] | watch <who>...|--all [--once]";

fn die(msg: &str) -> ! {
    eprintln!("{msg}");
    std::process::exit(1);
}
