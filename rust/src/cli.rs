// cli.rs — session-relay CLI (port of scripts/relay.mjs). The "doorbell" that
// wakes an idle session, plus manual registry/inbox ops over the shared store.
//
//   relay discover [--within <min>] [--tool claude|codex] [--exclude <id>] [--cwd <path>] [--json]
//   relay list
//   relay register <name> --id <uuid> [--dir <path>] [--tool claude|codex]
//   relay send <to> [--] <message...>            (or: send --id <id> [--] <message...>)
//   relay inbox <nameOrId>
//   relay wake <nameOrId> [--dry] [message...]
//   relay wake --id <id> --dir <cwd> --tool <claude|codex> [message...]
//
// `wake` is TOOL-AWARE: claude → `claude -p --resume <id> --output-format json -- <nudge>`,
// codex → `codex exec resume <id> --json -- <nudge>`, run from the target's
// registered project dir. `--dry` prints the command instead of spawning.

use crate::discover;
use crate::store;
use std::collections::HashMap;
use tinyjson::JsonValue;

const DEFAULT_NUDGE: &str = "You have new session-relay mail. Use the session-relay skill: call inbox to read your pending messages and act on them.";
const BOOL_FLAGS: [&str; 2] = ["dry", "json"];

fn die(msg: &str) -> ! {
    eprintln!("{msg}");
    std::process::exit(1);
}

struct Args(Vec<String>);

impl Args {
    // --name <value>; an empty value counts as absent (Node truthiness parity).
    fn flag(&self, name: &str) -> Option<&str> {
        let key = format!("--{name}");
        let i = self.0.iter().position(|a| *a == key)?;
        self.0
            .get(i + 1)
            .map(String::as_str)
            .filter(|v| !v.is_empty())
    }
    fn has(&self, name: &str) -> bool {
        self.0.iter().any(|a| a == &format!("--{name}"))
    }
    // positional args excluding flags + their values; a bare `--` ends option parsing.
    fn positionals(&self, from: usize) -> Vec<&str> {
        let mut out = Vec::new();
        let mut i = from;
        while i < self.0.len() {
            let a = &self.0[i];
            if a == "--" {
                break; // end-of-options: everything after is the verbatim message
            }
            if let Some(name) = a.strip_prefix("--") {
                if !BOOL_FLAGS.contains(&name) {
                    i += 1; // value flags also skip their value
                }
            } else {
                out.push(a.as_str());
            }
            i += 1;
        }
        out
    }
    // Message after an explicit `--` separator, verbatim; None when absent.
    fn message_after_sep(&self) -> Option<String> {
        let i = self.0.iter().position(|a| a == "--")?;
        Some(self.0[i + 1..].join(" "))
    }
}

struct Target {
    id: String,
    dir: Option<String>,
    tool: String,
    name: Option<String>,
}

// A target built straight from flags — addresses a discovered session that was
// never registered on the bus. The id MUST be a session UUID: it keeps an
// attacker-planted, flag-shaped id (e.g. "--config=…") off the doorbell argv.
fn explicit_target(args: &Args) -> Option<Target> {
    let id = args.flag("id")?;
    if !store::is_uuid(id) {
        die(&format!("--id must be a session UUID, got: {id}"));
    }
    Some(Target {
        id: id.to_string(),
        dir: Some(
            args.flag("dir")
                .map(str::to_string)
                .unwrap_or_else(cwd_string),
        ),
        tool: args.flag("tool").unwrap_or("claude").to_string(),
        name: None,
    })
}

fn from_entry(e: store::Entry) -> Target {
    Target {
        id: e.id,
        dir: e.dir,
        tool: e.tool,
        name: e.name,
    }
}

fn cwd_string() -> String {
    std::env::current_dir()
        .map(|d| d.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".to_string())
}

pub fn run(cmd: &str, raw: Vec<String>) -> ! {
    let args = Args(raw);
    match cmd {
        "discover" => {
            let within: f64 = args
                .flag("within")
                .and_then(|v| v.parse().ok())
                .filter(|v: &f64| v.is_finite())
                .unwrap_or(60.0);
            let rows = discover::discover(&discover::Options {
                active_within_min: within,
                tool: args.flag("tool"),
                exclude_id: args.flag("exclude"),
                cwd: args.flag("cwd"),
                ..Default::default()
            });
            if args.has("json") {
                println!(
                    "{}",
                    JsonValue::from(rows)
                        .format()
                        .unwrap_or_else(|_| "[]".into())
                );
                std::process::exit(0);
            }
            if rows.is_empty() {
                println!(
                    "(no active sessions in the last {} min)",
                    args.flag("within").unwrap_or("60")
                );
                std::process::exit(0);
            }
            for r in &rows {
                let o = r.get::<HashMap<String, JsonValue>>().expect("row object");
                let s = |k: &str| o.get(k).and_then(|v| v.get::<String>().cloned());
                let age = o
                    .get("ageSec")
                    .and_then(|v| v.get::<f64>().copied())
                    .unwrap_or(0.0) as i64;
                let registered = o
                    .get("registered")
                    .and_then(|v| v.get::<bool>().copied())
                    .unwrap_or(false);
                println!(
                    "[{:<6}] {}  {}  {}s ago{}{}",
                    s("tool").unwrap_or_default(),
                    s("id").unwrap_or_default(),
                    s("cwd").unwrap_or_else(|| "?".into()),
                    age,
                    s("name").map(|n| format!("  ({n})")).unwrap_or_default(),
                    if registered { "" } else { "  [unregistered]" },
                );
            }
            std::process::exit(0);
        }
        "list" => {
            let rows = store::roster();
            if rows.is_empty() {
                println!("(no sessions registered)");
                std::process::exit(0);
            }
            for r in rows {
                println!(
                    "{:<16} [{:<6}] {}  {}  {}",
                    r.name.as_deref().unwrap_or("(unnamed)"),
                    r.tool,
                    r.id,
                    r.dir.as_deref().unwrap_or("?"),
                    r.last_seen,
                );
            }
            std::process::exit(0);
        }
        "register" => {
            let pos = args.positionals(1);
            let (Some(name), Some(id)) = (pos.first(), args.flag("id")) else {
                die(
                    "usage: relay register <name> --id <uuid> [--dir <path>] [--tool claude|codex]",
                );
            };
            let dir = args
                .flag("dir")
                .map(str::to_string)
                .unwrap_or_else(cwd_string);
            match store::register(id, Some(&dir), Some(name), args.flag("tool")) {
                Ok(e) => {
                    println!(
                        "registered {} [{}] -> {} @ {}",
                        e.name.as_deref().unwrap_or(""),
                        e.tool,
                        e.id,
                        e.dir.as_deref().unwrap_or("")
                    );
                    std::process::exit(0);
                }
                Err(e) => die(&e),
            }
        }
        "send" => {
            let explicit = explicit_target(&args);
            let rest = args.positionals(1);
            let body = args.message_after_sep().unwrap_or_else(|| {
                if explicit.is_some() {
                    rest.join(" ")
                } else {
                    rest.iter().skip(1).copied().collect::<Vec<_>>().join(" ")
                }
            });
            let target = explicit.or_else(|| {
                rest.first()
                    .and_then(|to| store::resolve(to))
                    .map(from_entry)
            });
            let (Some(target), false) = (target, body.is_empty()) else {
                die(
                    "usage: relay send <to> [--] <message...>  (or: send --id <id> [--] <message...>)",
                );
            };
            let mut msg: HashMap<String, JsonValue> = HashMap::new();
            msg.insert("from".into(), JsonValue::from(()));
            msg.insert("fromName".into(), JsonValue::from("cli".to_string()));
            msg.insert("to".into(), JsonValue::from(target.id.clone()));
            msg.insert(
                "toName".into(),
                target
                    .name
                    .clone()
                    .map(JsonValue::from)
                    .unwrap_or(JsonValue::from(())),
            );
            msg.insert("body".into(), JsonValue::from(body));
            if let Err(e) = store::enqueue(&target.id, &msg) {
                die(&e);
            }
            println!("queued -> {}", target.name.as_deref().unwrap_or(&target.id));
            std::process::exit(0);
        }
        "inbox" => {
            let pos = args.positionals(1);
            let Some(who) = pos.first() else {
                die("usage: relay inbox <nameOrId>");
            };
            let Some(target) = store::resolve(who) else {
                die(&format!("unknown session: {who}"));
            };
            let msgs = store::drain(&target.id).unwrap_or_else(|e| die(&e));
            let mut out: HashMap<String, JsonValue> = HashMap::new();
            out.insert("count".into(), JsonValue::from(msgs.len() as f64));
            out.insert("messages".into(), JsonValue::from(msgs));
            println!(
                "{}",
                JsonValue::from(out)
                    .format()
                    .unwrap_or_else(|_| "{}".into())
            );
            std::process::exit(0);
        }
        "wake" => {
            let explicit = explicit_target(&args);
            let rest = args.positionals(1);
            let message = {
                let m = args.message_after_sep().unwrap_or_else(|| {
                    if explicit.is_some() {
                        rest.join(" ")
                    } else {
                        rest.iter().skip(1).copied().collect::<Vec<_>>().join(" ")
                    }
                });
                if m.is_empty() {
                    DEFAULT_NUDGE.to_string()
                } else {
                    m
                }
            };
            let target = explicit.or_else(|| {
                rest.first()
                    .and_then(|who| store::resolve(who))
                    .map(from_entry)
            });
            let Some(target) = target else {
                die(
                    "usage: relay wake <nameOrId> [message...]  |  wake --id <id> --dir <cwd> --tool <claude|codex> [message...]",
                );
            };
            let Some(dir) = target.dir.clone().filter(|d| !d.is_empty()) else {
                die("target missing id/dir (for an unregistered session pass --dir)");
            };
            // A registered target's id also lands on the spawned CLI's argv.
            // explicit_target() already UUID-gates an --id; gate the
            // resolved-name path too, so a planted, flag-shaped id in the
            // registry can't become an option.
            if !store::is_uuid(&target.id) {
                die(&format!(
                    "refusing to wake: target id is not a session UUID: {}",
                    target.id
                ));
            }
            // Per-tool headless-resume doorbell, run from the target's project
            // dir. The untrusted message goes AFTER a `--` end-of-options
            // marker so a dash-leading body can't be parsed as a flag on the
            // child (both CLIs take the prompt as a trailing positional).
            let (cmd, cargs): (&str, Vec<&str>) = if target.tool == "codex" {
                (
                    "codex",
                    vec!["exec", "resume", &target.id, "--json", "--", &message],
                )
            } else {
                (
                    "claude",
                    vec![
                        "-p",
                        "--resume",
                        &target.id,
                        "--output-format",
                        "json",
                        "--",
                        &message,
                    ],
                )
            };
            if args.has("dry") {
                let mut m: HashMap<String, JsonValue> = HashMap::new();
                m.insert("tool".into(), JsonValue::from(target.tool.clone()));
                m.insert("cmd".into(), JsonValue::from(cmd.to_string()));
                m.insert(
                    "args".into(),
                    JsonValue::from(
                        cargs
                            .iter()
                            .map(|a| JsonValue::from(a.to_string()))
                            .collect::<Vec<_>>(),
                    ),
                );
                m.insert("cwd".into(), JsonValue::from(dir.clone()));
                println!(
                    "{}",
                    JsonValue::from(m)
                        .stringify()
                        .unwrap_or_else(|_| "{}".into())
                );
                std::process::exit(0);
            }
            // Never resume into a cwd that no longer exists: a stale/moved
            // registration would otherwise resume from an unexpected dir (and
            // Codex widens its sandbox writable roots to the caller cwd).
            if !std::path::Path::new(&dir).exists() {
                die(&format!(
                    "target dir does not exist: {dir} — stale/moved session; re-register or pass the current --dir before waking."
                ));
            }
            let out = std::process::Command::new(cmd)
                .args(&cargs)
                .current_dir(&dir)
                .output()
                .unwrap_or_else(|e| die(&format!("failed to spawn {cmd}: {e}")));
            let stdout = String::from_utf8_lossy(&out.stdout);
            if !stdout.is_empty() {
                if stdout.ends_with('\n') {
                    print!("{stdout}");
                } else {
                    println!("{stdout}");
                }
            }
            if !out.stderr.is_empty() {
                eprint!("{}", String::from_utf8_lossy(&out.stderr));
            }
            std::process::exit(out.status.code().unwrap_or(0));
        }
        _ => die(
            "usage: relay discover [--within min] [--tool t] | list | register <name> --id <uuid> [--dir <path>] | send <to> <msg> | inbox <who> | wake <who> [msg]",
        ),
    }
}
