// cli.rs — session-relay CLI (port of scripts/relay.mjs). The "doorbell" that
// wakes an idle session, plus manual registry/inbox ops over the shared store.
//
//   relay discover [--within <min>] [--tool claude|codex] [--exclude <id>] [--cwd <path>] [--json]
//   relay list
//   relay register <name> --id <uuid> [--dir <path>] [--tool claude|codex]
//   relay send <to> [--] <message...>            (or: send --id <id> [--] <message...>)
//   relay inbox <nameOrId>
//   relay peek <nameOrId>                        (read-only: inbox without draining)
//   relay attach <nameOrId> [--exec]             (interactive human takeover)
//   relay wake <nameOrId> [--model <m>] [--effort <e>] [--dry] [message...]
//   relay wake --id <id> --dir <cwd> --tool <claude|codex> [--model <m>] [--effort <e>] [message...]
//
// `wake` is TOOL-AWARE: claude → `claude -p --resume <id> [--model m] [--effort e] --output-format json -- <nudge>`,
// codex → `codex exec resume <id> [-m m] [-c model_reasoning_effort=e] --json -- <nudge>`,
// run from the target's registered project dir. `--dry` prints the command
// instead of spawning.

use crate::discover;
use crate::hook;
use crate::store;
use std::collections::HashMap;
use std::io::Write;
use std::os::unix::process::CommandExt;
use tinyjson::JsonValue;

pub(crate) const DEFAULT_NUDGE: &str = "You have new session-relay mail. Use the session-relay skill: call inbox to read your pending messages and act on them.";
const BOOL_FLAGS: [&str; 8] = [
    "dry",
    "json",
    "auto-turn",
    "once",
    "all",
    "read-only",
    "full-access",
    "watch",
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
    last_seen: String,
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
        last_seen: "unregistered".to_string(),
    })
}

fn from_entry(e: store::Entry) -> Target {
    Target {
        id: e.id,
        dir: e.dir,
        tool: e.tool,
        name: e.name,
        last_seen: e.last_seen,
    }
}

fn cwd_string() -> String {
    std::env::current_dir()
        .map(|d| d.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".to_string())
}

const ATTACH_WARNING: &str = "WARNING: split-brain risk — neither CLI locks sessions; attaching while automation drives the session interleaves two writers. Prefer attach when the worker is idle; relay doctor --id <id> shows watcher/lock state.";

struct AttachInvocation {
    program: String,
    args: Vec<String>,
    cwd: Option<String>,
    rendered: String,
}

struct ParsedAttachArgs {
    target: String,
    execute: bool,
}

fn parse_attach_args(raw: &[String]) -> Result<ParsedAttachArgs, ()> {
    let mut target = None;
    let mut execute = false;
    let mut options = true;
    for arg in raw.iter().skip(1) {
        if options && arg == "--" {
            options = false;
            continue;
        }
        if options && arg == "--exec" {
            if execute {
                return Err(());
            }
            execute = true;
            continue;
        }
        if options && arg.starts_with('-') {
            return Err(());
        }
        if arg.is_empty() {
            return Err(());
        }
        if target.replace(arg.clone()).is_some() {
            return Err(());
        }
    }
    let Some(target) = target else {
        return Err(());
    };
    Ok(ParsedAttachArgs { target, execute })
}

fn shell_word(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'-'))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}

fn attach_invocation(
    tool: &str,
    id: &str,
    dir: Option<&str>,
    dir_exists: bool,
    server: Option<&str>,
) -> Result<AttachInvocation, String> {
    match tool {
        "codex" => {
            let mut args = if let Some(server) = server {
                vec!["--remote".to_string(), format!("unix://{server}")]
            } else {
                vec!["resume".to_string(), id.to_string()]
            };
            if server.is_none() && dir_exists {
                if let Some(dir) = dir {
                    args.push("-C".to_string());
                    args.push(dir.to_string());
                }
            }
            let rendered = std::iter::once("codex".to_string())
                .chain(args.iter().map(|arg| shell_word(arg)))
                .collect::<Vec<_>>()
                .join(" ");
            Ok(AttachInvocation {
                program: "codex".to_string(),
                args,
                cwd: None,
                rendered,
            })
        }
        "claude" => {
            let args = vec!["--resume".to_string(), id.to_string()];
            let command = format!("claude --resume {}", shell_word(id));
            let rendered = dir
                .map(|dir| format!("cd {} && {command}", shell_word(dir)))
                .unwrap_or(command);
            Ok(AttachInvocation {
                program: "claude".to_string(),
                args,
                cwd: dir.map(str::to_string),
                rendered,
            })
        }
        _ => Err(format!(
            "attach target tool must be claude|codex, got: {tool}"
        )),
    }
}

fn discovered_target(id: &str) -> Option<Target> {
    let rows = discover::discover(&discover::Options {
        active_within_min: 100.0 * 365.25 * 24.0 * 60.0,
        limit: usize::MAX,
        ..Default::default()
    });
    rows.into_iter().find_map(|row| {
        let object = row.get::<HashMap<String, JsonValue>>()?;
        let string = |key: &str| object.get(key)?.get::<String>().cloned();
        (string("id").as_deref() == Some(id)).then(|| Target {
            id: id.to_string(),
            dir: string("cwd"),
            tool: string("tool").unwrap_or_default(),
            name: string("name"),
            last_seen: string("lastActivity").unwrap_or_else(|| "unknown".to_string()),
        })
    })
}

fn attach(args: &Args) -> ! {
    let parsed = match parse_attach_args(&args.0) {
        Ok(parsed) => parsed,
        Err(()) => {
            eprintln!("{ATTACH_WARNING}");
            eprintln!("usage: relay attach <nameOrId> [--exec]");
            std::process::exit(2);
        }
    };
    let who = parsed.target.as_str();
    let target = match store::resolve(who) {
        Some(entry) => from_entry(entry),
        None if store::is_uuid(who) => discovered_target(who).unwrap_or_else(|| {
            eprintln!("{ATTACH_WARNING}");
            die(&format!("unknown session UUID: {who}"));
        }),
        None => {
            eprintln!("{ATTACH_WARNING}");
            die(&format!("unknown session name or non-session UUID: {who}"));
        }
    };
    if !store::is_uuid(&target.id) {
        eprintln!("{ATTACH_WARNING}");
        die(&format!(
            "refusing to attach: target id is not a session UUID: {}",
            target.id
        ));
    }
    if !matches!(target.tool.as_str(), "claude" | "codex") {
        eprintln!("{ATTACH_WARNING}");
        die(&format!(
            "attach target tool must be claude|codex, got: {}",
            target.tool
        ));
    }
    match store::resume_status(&target.id) {
        store::LockStatus::Live => {
            eprintln!("{ATTACH_WARNING}");
            eprintln!(
                "attach refused: relay wake is in flight for {} (resume lock held)",
                target.name.as_deref().unwrap_or(&target.id)
            );
            std::process::exit(3);
        }
        store::LockStatus::Unknown => {
            eprintln!("{ATTACH_WARNING}");
            eprintln!(
                "attach refused: cannot verify resume lock state for {}. Run relay doctor --id {} and restore lock access; remove a stale lock only after confirming no wake is running.",
                target.name.as_deref().unwrap_or(&target.id),
                target.id
            );
            std::process::exit(4);
        }
        store::LockStatus::Dead | store::LockStatus::Never => {}
    }

    let dir_exists = target
        .dir
        .as_deref()
        .is_some_and(|dir| std::path::Path::new(dir).is_dir());
    // The live-view plan will pass a registered app-server socket here once
    // its optional `server` schema lands. Attach composition is ready now;
    // current registry entries intentionally have no server field.
    let invocation = attach_invocation(
        &target.tool,
        &target.id,
        target.dir.as_deref(),
        dir_exists,
        None,
    )
    .unwrap_or_else(|e| {
        eprintln!("{ATTACH_WARNING}");
        die(&e)
    });

    if parsed.execute {
        eprintln!("{ATTACH_WARNING}");
        let Some(_) = target.dir.as_deref().filter(|_| dir_exists) else {
            die("attach --exec refused: stored dir does not exist");
        };
        let mut command = std::process::Command::new(&invocation.program);
        command.args(&invocation.args);
        if let Some(cwd) = invocation.cwd.as_deref() {
            command.current_dir(cwd);
        }
        let error = command.exec();
        die(&format!(
            "attach --exec failed to replace process with {}: {error}",
            invocation.program
        ));
    }

    println!(
        "session: name={} tool={} dir={} last_seen={}",
        target.name.as_deref().unwrap_or("(unnamed)"),
        target.tool,
        target.dir.as_deref().unwrap_or("?"),
        target.last_seen
    );
    println!("command: {}", invocation.rendered);
    println!("{ATTACH_WARNING}");
    std::process::exit(0);
}

fn lock_age(path: &std::path::Path) -> String {
    let Ok(age) = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .and_then(|t| t.elapsed().map_err(std::io::Error::other))
    else {
        return "unknown time".to_string();
    };
    let seconds = age.as_secs();
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else {
        format!("{}h", seconds / 3600)
    }
}

fn progress_age(ms: i64) -> String {
    let seconds = (ms.max(0) as u64) / 1000;
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else {
        format!("{}h", seconds / 3600)
    }
}

fn doctor_line(level: &str, check: &str, detail: &str) {
    println!("{level} {check}: {detail}");
}

fn doctor(args: &Args) -> ! {
    let cwd = cwd_string();
    let (id, fallback) = match args.flag("id") {
        Some(who) => match store::resolve(who) {
            Some(entry) => (entry.id, false),
            None if store::is_uuid(who) => (who.to_string(), false),
            None => {
                doctor_line(
                    "FAIL",
                    "identity",
                    &format!("unknown session {who} — fix: relay list"),
                );
                std::process::exit(1);
            }
        },
        None => match store::id_for_dir(&cwd) {
            Some(id) => (id, true),
            None => {
                doctor_line(
                    "FAIL",
                    "identity",
                    "no cwd marker — fix: pass --id <session-id-or-name>",
                );
                std::process::exit(1);
            }
        },
    };

    if fallback {
        doctor_line(
            "WARN",
            "identity",
            &format!(
                "single-session-only fallback resolved {id} from {cwd}; pass --id for shared dirs"
            ),
        );
    } else {
        doctor_line("PASS", "identity", &id);
    }

    let mut failures = 0;
    let entry = store::resolve(&id);
    if let Some(entry) = &entry {
        doctor_line(
            "PASS",
            "registration",
            &format!(
                "{} [{}] at {}",
                entry.name.as_deref().unwrap_or(&entry.id),
                entry.tool,
                entry.dir.as_deref().unwrap_or("unknown dir")
            ),
        );
    } else {
        failures += 1;
        doctor_line(
            "FAIL",
            "registration",
            "registry entry missing — fix: restart or resume the session",
        );
    }

    let mailbox = store::mailbox_path(&id);
    match std::fs::File::open(&mailbox) {
        Ok(_) => doctor_line(
            "PASS",
            "mailbox",
            &format!("readable {}", mailbox.display()),
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => doctor_line(
            "PASS",
            "mailbox",
            &format!("no mail yet ({})", mailbox.display()),
        ),
        Err(e) => {
            failures += 1;
            doctor_line(
                "FAIL",
                "mailbox",
                &format!(
                    "cannot read {} ({e}) — fix: restore mailbox read permissions",
                    mailbox.display()
                ),
            );
        }
    }

    let relay_exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "relay".to_string());
    let rearm = hook::watcher_command(&relay_exe, &id);
    let watch = store::watcher_status(&id);
    match watch {
        store::LockStatus::Live => doctor_line("PASS", "watcher", "live lock held"),
        store::LockStatus::Dead | store::LockStatus::Never => {
            failures += 1;
            doctor_line(
                "FAIL",
                "watcher",
                &format!("{} — fix: {rearm}", watch.as_str()),
            );
        }
        store::LockStatus::Unknown => {
            failures += 1;
            doctor_line(
                "FAIL",
                "watcher",
                &format!("unknown lock state — fix: {rearm}"),
            );
        }
    }

    match store::watcher_progress_age_ms(&id) {
        Some(ms) if ms > store::WATCH_PROGRESS_STALE_MS => doctor_line(
            "WARN",
            "watcher-progress",
            &format!("last update {} ago; watcher may be stuck", progress_age(ms)),
        ),
        Some(ms) => doctor_line(
            "PASS",
            "watcher-progress",
            &format!("updated {} ago", progress_age(ms)),
        ),
        None => doctor_line("WARN", "watcher-progress", "no progress stamp yet"),
    }

    match store::resume_status(&id) {
        store::LockStatus::Live => doctor_line("PASS", "resume", "relay wake is running"),
        store::LockStatus::Dead => {
            doctor_line("PASS", "resume", "no active wake (prior tombstone)")
        }
        store::LockStatus::Never => doctor_line("PASS", "resume", "no relay wake recorded"),
        store::LockStatus::Unknown => doctor_line("WARN", "resume", "lock state unknown"),
    }

    match store::with_lock(|| Ok(())) {
        Ok(()) => doctor_line("PASS", "store-lock", "acquired and released"),
        Err(e) => {
            failures += 1;
            doctor_line(
                "FAIL",
                "store-lock",
                &format!("{e} — fix: inspect {}", store::home_dir().display()),
            );
        }
    }

    std::process::exit(if failures == 0 { 0 } else { 1 });
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
        "attach" => attach(&args),
        "doctor" => doctor(&args),
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
            let resume_path = store::resume_lock_path(&target.id);
            let _resume_guard = match store::acquire_resume_lock(&target.id, &target.tool) {
                Ok(guard) => guard,
                Err(store::LockAcquireError::Busy(metadata)) => {
                    let addressee = target.name.as_deref().unwrap_or(&target.id);
                    let pid = metadata
                        .as_ref()
                        .map(|m| m.pid.to_string())
                        .unwrap_or_else(|| "unknown".to_string());
                    eprintln!(
                        "wake refused: resume already running for {addressee} (pid {pid}, started {} ago)",
                        lock_age(&resume_path)
                    );
                    std::process::exit(3);
                }
                Err(store::LockAcquireError::Io(e)) => {
                    die(&format!("cannot acquire wake resume lock: {e}"));
                }
            };
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
            "usage: relay discover [--within min] [--tool t] | list | register <name> --id <uuid> [--dir <path>] | send <to> <msg> | inbox <who> | peek <who> | attach <who> [--exec] | wake <who> [--model m] [--effort e] [msg] | doctor [--id <session>]",
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

    #[test]
    fn attach_codex_uses_resume_and_existing_registered_dir() {
        let attach = attach_invocation(
            "codex",
            "11111111-1111-4111-8111-111111111111",
            Some("/tmp/project with space"),
            true,
            None,
        )
        .unwrap();
        assert_eq!(attach.program, "codex");
        assert_eq!(
            attach.args,
            strings(&[
                "resume",
                "11111111-1111-4111-8111-111111111111",
                "-C",
                "/tmp/project with space"
            ])
        );
        assert_eq!(
            attach.rendered,
            "codex resume 11111111-1111-4111-8111-111111111111 -C '/tmp/project with space'"
        );
    }

    #[test]
    fn attach_claude_changes_to_the_registered_dir() {
        let attach = attach_invocation(
            "claude",
            "22222222-2222-4222-8222-222222222222",
            Some("/tmp/project with space"),
            true,
            None,
        )
        .unwrap();
        assert_eq!(attach.program, "claude");
        assert_eq!(
            attach.args,
            strings(&["--resume", "22222222-2222-4222-8222-222222222222"])
        );
        assert_eq!(attach.cwd.as_deref(), Some("/tmp/project with space"));
        assert_eq!(
            attach.rendered,
            "cd '/tmp/project with space' && claude --resume 22222222-2222-4222-8222-222222222222"
        );
    }

    #[test]
    fn attach_codex_server_seam_prefers_remote_over_resume() {
        let attach = attach_invocation(
            "codex",
            "33333333-3333-4333-8333-333333333333",
            Some("/tmp/project"),
            true,
            Some("/tmp/codex app.sock"),
        )
        .unwrap();
        assert_eq!(attach.program, "codex");
        assert_eq!(
            attach.args,
            strings(&["--remote", "unix:///tmp/codex app.sock"])
        );
        assert_eq!(
            attach.rendered,
            "codex --remote 'unix:///tmp/codex app.sock'"
        );
    }

    #[test]
    fn attach_parser_accepts_one_target_and_exec_only_before_terminator() {
        let before = parse_attach_args(&strings(&["attach", "--exec", "worker"])).unwrap();
        assert_eq!(before.target, "worker");
        assert!(before.execute);

        let after = parse_attach_args(&strings(&["attach", "worker", "--exec"])).unwrap();
        assert_eq!(after.target, "worker");
        assert!(after.execute);

        assert!(parse_attach_args(&strings(&["attach", "worker", "extra"])).is_err());
        assert!(parse_attach_args(&strings(&["attach", "worker", "--bogus"])).is_err());
        assert!(parse_attach_args(&strings(&["attach", "worker", "--", "--exec"])).is_err());
    }
}
