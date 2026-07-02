// relay — session-relay's single binary. One executable, multi-call:
//   relay bus                      MCP stdio server (manifest entry)
//   relay hook [codex] [--event prompt]   SessionStart/UserPromptSubmit hook (register + drain inbox)
//   relay discover|list|register|send|inbox|peek|wake   the CLI / doorbell
//   relay watch …                  poll mailboxes, push into live Codex threads via app-server
//   relay __stress …               hidden test helper (cross-process lock race)

use std::collections::HashMap;
use tinyjson::JsonValue;

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match argv.first().map(String::as_str) {
        Some("bus") => relay::bus::run(),
        Some("hook") => relay::hook::run(&argv[1..]),
        Some(cmd @ ("discover" | "list" | "register" | "send" | "inbox" | "peek" | "wake")) => {
            relay::cli::run(cmd, argv.clone())
        }
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
        _ => die(
            "usage: relay bus | hook [codex] [--event prompt] | discover [--within min] [--tool t] | list | register <name> --id <uuid> [--dir <path>] | send <to> [--] <msg> | inbox <who> | peek <who> | wake <who> [msg] | watch <who>...|--all [--server <sock>] [--auto-turn] [--once]",
        ),
    }
}

fn die(msg: &str) -> ! {
    eprintln!("{msg}");
    std::process::exit(1);
}
