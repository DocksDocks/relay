// cli.rs — session-relay CLI (port of scripts/relay.mjs). The "doorbell" that
// wakes an idle session, plus manual registry/inbox ops over the shared store.
//
//   relay discover [--within <min>] [--tool claude|codex] [--exclude <id>] [--cwd <path>] [--json]
//   relay list
//   relay register <name> --id <uuid> [--dir <path>] [--tool claude|codex]
//   relay send <to> [--] <message...>            (or: send --id <id> [--] <message...>)
//   relay inbox <nameOrId>
//   relay peek <nameOrId>                        (read-only: inbox without draining)
//   relay wake <nameOrId> [--model <m>] [--effort <e>] [--dry] [message...]
//   relay wake --id <id> --dir <cwd> --tool <claude|codex> [--model <m>] [--effort <e>] [message...]
//
// `wake` is TOOL-AWARE: claude → `claude -p --resume <id> [--model m] [--effort e] --output-format json -- <nudge>`,
// codex → `codex exec resume <id> [-m m] [-c model_reasoning_effort=e] --json -- <nudge>`,
// run from the target's registered project dir. `--dry` prints the command
// instead of spawning.

use crate::discover;
use crate::store;
use std::collections::HashMap;
use std::io::Write;
use tinyjson::JsonValue;

pub(crate) const DEFAULT_NUDGE: &str = "You have new session-relay mail. Use the session-relay skill: call inbox to read your pending messages and act on them.";
const BOOL_FLAGS: [&str; 7] = [
    "dry",
    "json",
    "auto-turn",
    "once",
    "all",
    "read-only",
    "full-access",
];

fn die(msg: &str) -> ! {
    eprintln!("{msg}");
    std::process::exit(1);
}

pub(crate) struct Args(pub(crate) Vec<String>);

impl Args {
    // --name <value>; an empty value counts as absent (Node truthiness parity).
    pub(crate) fn flag(&self, name: &str) -> Option<&str> {
        let key = format!("--{name}");
        let i = self.0.iter().position(|a| *a == key)?;
        self.0
            .get(i + 1)
            .map(String::as_str)
            .filter(|v| !v.is_empty())
    }
    pub(crate) fn has(&self, name: &str) -> bool {
        self.0.iter().any(|a| a == &format!("--{name}"))
    }
    // positional args excluding flags + their values; a bare `--` ends option parsing.
    pub(crate) fn positionals(&self, from: usize) -> Vec<&str> {
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
    pub(crate) fn message_after_sep(&self) -> Option<String> {
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

fn wake_cmd(tool: &str) -> String {
    let var = if tool == "codex" {
        "RELAY_WAKE_CMD_CODEX"
    } else {
        "RELAY_WAKE_CMD_CLAUDE"
    };
    std::env::var(var)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| {
            if tool == "codex" {
                "codex".to_string()
            } else {
                "claude".to_string()
            }
        })
}

fn doorbell_args(
    tool: &str,
    id: &str,
    message: &str,
    model: Option<&str>,
    effort: Option<&str>,
) -> (String, Vec<String>) {
    let mut cargs = if tool == "codex" {
        vec!["exec".into(), "resume".into(), id.into()]
    } else {
        vec!["-p".into(), "--resume".into(), id.into()]
    };
    if let Some(model) = model {
        cargs.push(if tool == "codex" { "-m" } else { "--model" }.into());
        cargs.push(model.into());
    }
    if let Some(effort) = effort {
        if tool == "codex" {
            cargs.push("-c".into());
            cargs.push(format!("model_reasoning_effort={effort}"));
        } else {
            cargs.push("--effort".into());
            cargs.push(effort.into());
        }
    }
    if tool == "codex" {
        cargs.push("--json".into());
    } else {
        cargs.push("--output-format".into());
        cargs.push("json".into());
    }
    cargs.push("--".into());
    cargs.push(message.into());
    (wake_cmd(tool), cargs)
}

#[derive(Debug, PartialEq, Eq)]
struct WakeUsage {
    input_tokens: u64,
    cached_input_tokens: Option<u64>,
    output_tokens: u64,
    cost_usd: Option<String>,
}

impl WakeUsage {
    fn render(&self, tool: &str) -> String {
        let mut line = format!("[relay wake] {tool}: {} in", self.input_tokens);
        if let Some(cached) = self.cached_input_tokens.filter(|n| *n > 0) {
            line.push_str(&format!(" ({cached} cached)"));
        }
        line.push_str(&format!(" / {} out", self.output_tokens));
        if let Some(cost) = &self.cost_usd {
            line.push_str(&format!(", ${cost}"));
        }
        line
    }
}

fn obj(v: &JsonValue) -> Option<&HashMap<String, JsonValue>> {
    v.get::<HashMap<String, JsonValue>>()
}

fn str_field<'a>(o: &'a HashMap<String, JsonValue>, k: &str) -> Option<&'a str> {
    o.get(k)?.get::<String>().map(String::as_str)
}

fn num_field(o: &HashMap<String, JsonValue>, k: &str) -> Option<u64> {
    let n = o.get(k)?.get::<f64>().copied()?;
    (n.is_finite() && n >= 0.0).then_some(n as u64)
}

fn cost_field(o: &HashMap<String, JsonValue>, k: &str) -> Option<String> {
    let n = o.get(k)?.get::<f64>().copied()?;
    (n.is_finite() && n >= 0.0).then_some(n.to_string())
}

fn claude_usage(root: &HashMap<String, JsonValue>) -> Option<WakeUsage> {
    if str_field(root, "type")? != "result" {
        return None;
    }
    let usage = root.get("usage").and_then(obj)?;
    let prompt = num_field(usage, "input_tokens")?;
    let cache_read = num_field(usage, "cache_read_input_tokens");
    let cache_create = num_field(usage, "cache_creation_input_tokens").unwrap_or(0);
    Some(WakeUsage {
        input_tokens: prompt + cache_read.unwrap_or(0) + cache_create,
        cached_input_tokens: cache_read,
        output_tokens: num_field(usage, "output_tokens")?,
        cost_usd: cost_field(root, "total_cost_usd"),
    })
}

fn codex_usage_line(stdout: &str) -> Option<WakeUsage> {
    for line in stdout.lines() {
        let root = line.parse::<JsonValue>().ok()?;
        let root = obj(&root)?;
        if str_field(root, "type") != Some("turn.completed") {
            continue;
        }
        let usage = root.get("usage").and_then(obj)?;
        let visible_output = num_field(usage, "output_tokens")?;
        let reasoning_output = num_field(usage, "reasoning_output_tokens").unwrap_or(0);
        return Some(WakeUsage {
            input_tokens: num_field(usage, "input_tokens")?,
            cached_input_tokens: num_field(usage, "cached_input_tokens"),
            output_tokens: visible_output + reasoning_output,
            cost_usd: None,
        });
    }
    None
}

fn wake_usage_line(tool: &str, stdout: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(stdout).ok()?;
    let usage = if tool == "codex" {
        codex_usage_line(text)?
    } else {
        let root = text.parse::<JsonValue>().ok()?;
        let root = obj(&root)?;
        claude_usage(root)?
    };
    Some(usage.render(tool))
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
            // --from names the sender's own registered session (id or name):
            // the CLI otherwise has no identity and mail lands as "cli".
            let (from_id, from_name) = match args.flag("from") {
                Some(f) => {
                    let Some(sender) = store::resolve(f) else {
                        die(&format!("unknown --from identity: {f}"));
                    };
                    (
                        JsonValue::from(sender.id),
                        JsonValue::from(sender.name.unwrap_or_else(|| "cli".to_string())),
                    )
                }
                None => (JsonValue::from(()), JsonValue::from("cli".to_string())),
            };
            let mut msg: HashMap<String, JsonValue> = HashMap::new();
            msg.insert("from".into(), from_id);
            msg.insert("fromName".into(), from_name);
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
        "peek" => {
            let pos = args.positionals(1);
            let Some(who) = pos.first() else {
                die("usage: relay peek <nameOrId>");
            };
            let Some(target) = store::resolve(who) else {
                die(&format!("unknown session: {who}"));
            };
            let msgs = store::peek(&target.id);
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
                    "usage: relay wake <nameOrId> [--model <m>] [--effort <e>] [message...]  |  wake --id <id> --dir <cwd> --tool <claude|codex> [--model <m>] [--effort <e>] [message...]",
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
            let model = args.flag("model");
            let effort = args.flag("effort");
            if model.is_none() {
                eprintln!(
                    "[relay wake] no --model given — pass --model/--effort to pin a deliberate doorbell model"
                );
            }
            let (cmd, cargs) = doorbell_args(&target.tool, &target.id, &message, model, effort);
            if args.has("dry") {
                let mut m: HashMap<String, JsonValue> = HashMap::new();
                m.insert("tool".into(), JsonValue::from(target.tool.clone()));
                m.insert("cmd".into(), JsonValue::from(cmd.clone()));
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
            let out = std::process::Command::new(&cmd)
                .args(&cargs)
                .current_dir(&dir)
                .output()
                .unwrap_or_else(|e| die(&format!("failed to spawn {cmd}: {e}")));
            if !out.stdout.is_empty() {
                let _ = std::io::stdout().write_all(&out.stdout);
            }
            if !out.stderr.is_empty() {
                eprint!("{}", String::from_utf8_lossy(&out.stderr));
            }
            if let Some(line) = wake_usage_line(&target.tool, &out.stdout) {
                eprintln!("{line}");
            }
            std::process::exit(out.status.code().unwrap_or(0));
        }
        _ => die(
            "usage: relay discover [--within min] [--tool t] | list | register <name> --id <uuid> [--dir <path>] | send <to> <msg> | inbox <who> | peek <who> | wake <who> [--model m] [--effort e] [msg]",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn wake_argv_no_flags_match_existing_shapes() {
        let (cmd, args) = doorbell_args("codex", "u-1", "ping", None, None);
        assert_eq!(cmd, "codex");
        assert_eq!(
            args,
            strings(&["exec", "resume", "u-1", "--json", "--", "ping"])
        );

        let (cmd, args) = doorbell_args("claude", "u-1", "ping", None, None);
        assert_eq!(cmd, "claude");
        assert_eq!(
            args,
            strings(&[
                "-p",
                "--resume",
                "u-1",
                "--output-format",
                "json",
                "--",
                "ping"
            ])
        );
    }

    #[test]
    fn wake_argv_maps_model_and_effort_per_tool_after_resume_id() {
        let (cmd, args) = doorbell_args("codex", "u-1", "ping", Some("gpt-5.5"), Some("xhigh"));
        assert_eq!(cmd, "codex");
        assert_eq!(
            args,
            strings(&[
                "exec",
                "resume",
                "u-1",
                "-m",
                "gpt-5.5",
                "-c",
                "model_reasoning_effort=xhigh",
                "--json",
                "--",
                "ping"
            ])
        );

        let (cmd, args) = doorbell_args("claude", "u-1", "ping", Some("opus"), Some("max"));
        assert_eq!(cmd, "claude");
        assert_eq!(
            args,
            strings(&[
                "-p",
                "--resume",
                "u-1",
                "--model",
                "opus",
                "--effort",
                "max",
                "--output-format",
                "json",
                "--",
                "ping"
            ])
        );
    }

    #[test]
    fn parses_claude_fixture_usage_line() {
        let line = wake_usage_line(
            "claude",
            include_str!("../../test/fixtures/wake-usage-claude.json").as_bytes(),
        );
        assert_eq!(
            line,
            Some("[relay wake] claude: 45682 in (45603 cached) / 4 out, $0.0142089".to_string())
        );
    }

    #[test]
    fn parses_codex_fixture_usage_line() {
        let line = wake_usage_line(
            "codex",
            include_str!("../../test/fixtures/wake-usage-codex.json").as_bytes(),
        );
        assert_eq!(
            line,
            Some("[relay wake] codex: 47400 in (12032 cached) / 10 out".to_string())
        );
    }

    #[test]
    fn parser_accepts_no_trailing_newline_payload() {
        let line = wake_usage_line(
            "claude",
            br#"{"type":"result","total_cost_usd":1.25,"usage":{"input_tokens":7,"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"output_tokens":3}}"#,
        );
        assert_eq!(
            line,
            Some("[relay wake] claude: 7 in / 3 out, $1.25".to_string())
        );
    }

    #[test]
    fn parser_ignores_invalid_utf8_stdout() {
        assert_eq!(
            wake_usage_line("claude", b"{\"type\":\"result\"}\xff"),
            None
        );
    }

    #[test]
    fn parser_ignores_garbage_stdout() {
        assert_eq!(wake_usage_line("codex", b"not json"), None);
    }
}
