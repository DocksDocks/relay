// relay — session-relay's single binary. One executable, multi-call:
//   relay bus                      MCP stdio server (manifest entry)
//   relay channel                  EXPERIMENTAL one-way Claude channel MCP server
//   relay hook [codex] [--event prompt]   SessionStart/UserPromptSubmit hook (register + drain inbox)
//   relay discover|list|register|send|inbox|peek|attach|wake|doctor   CLI / attach / doorbell / health
//   relay watch …                  poll mailboxes, push into live Codex threads via app-server
//   relay __spawn-log-writer <id>  hidden bounded stderr pump for detached spawn
//   relay __appserver-spawn-pump    hidden bounded app-server first-turn pump
//   relay __lifecycle-watchdog …    hidden detached supervisor owner
//   relay __lifecycle-supervisor …  hidden exact child custodian
//   relay __stress …               hidden test helper (cross-process lock race)

use std::collections::HashMap;
use tinyjson::JsonValue;

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match argv.first().map(String::as_str) {
        Some("bus") => relay::bus::run(),
        Some("channel") => relay::channel::run(),
        Some("hook") => relay::hook::run(&argv[1..]),
        Some(
            cmd @ ("discover" | "list" | "register" | "send" | "inbox" | "peek" | "attach" | "wake"
            | "doctor"),
        ) => relay::cli::run(cmd, argv.clone()),
        Some("watch") => relay::watch::run(argv.clone()),
        Some("spawn") => relay::spawn::run(argv.clone()),
        Some("__spawn-log-writer") => {
            let Some(id) = argv.get(1) else {
                die("usage: relay __spawn-log-writer <uuid>");
            };
            relay::spawn::run_log_writer(id);
        }
        Some("__appserver-spawn-pump") => relay::spawn::run_appserver_pump(),
        Some("__lifecycle-watchdog") => {
            relay::supervisor::run_watchdog(&argv[1..]).unwrap_or_else(|error| die(&error))
        }
        Some("__lifecycle-supervisor") => {
            relay::supervisor::run_supervisor(&argv[1..]).unwrap_or_else(|error| die(&error))
        }
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
                relay::store::register(who, Some(&format!("/tmp/{who}")), Some(who), None, None)
                    .unwrap_or_else(|e| die(&e));
                relay::store::register(&format!("{who}-op{i}"), Some("/tmp/x"), None, None, None)
                    .unwrap_or_else(|e| die(&e));
            }
        }
        _ => die(
            "usage: relay bus | channel | hook [codex] [--event prompt] | discover [--within min] [--tool t] | list | register <name> --id <uuid> [--dir <path>] [--server <sock>] | send <to> [--] <msg> | inbox <who> | peek <who> | attach <who> [--exec] | wake <who> [--model m] [--effort e] [msg] | doctor [--id <session>] | watch <who>...|--all [--server <sock>] [--auto-turn] [--once] | spawn <dir> [--tool t] [--model m] [--effort e] [--name n] [--server <sock>] [--watch] [--] <task>",
        ),
    }
}

fn die(msg: &str) -> ! {
    eprintln!("{msg}");
    std::process::exit(1);
}
