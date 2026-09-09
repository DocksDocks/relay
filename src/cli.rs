// cli.rs — session-relay CLI (port of scripts/relay.mjs). The "doorbell" that
// wakes an idle session, plus manual registry/inbox ops over the shared store.
//
//   relay discover [--within <min>] [--tool omp] [--exclude <id>] [--cwd <path>] [--json]
//   relay list
//   relay register <name> --id <uuid> [--dir <path>] [--tool omp]
//   relay send <to> [--] <message...>            (or: send --id <id> [--] <message...>)
//   relay request <to> [--from <registered>] [--json] [--] <message...>
//   relay reply <correlation-id> [--from <registered>] --status completed|failed [--] <message...>
//   relay inbox [--hold [<seconds>]] <nameOrId>
//   relay ack <token> | rollback <token>
//   relay peek <nameOrId>                        (read-only: inbox without draining)
//   relay attach <nameOrId> [--exec]             (interactive human takeover)
//   relay wake <nameOrId> [--model <m>] [--effort <e>] [--dry] [message...]
//   relay wake --id <id> --dir <cwd> --tool omp [--model <m>] [--effort <e>] [message...]
//
// Wake runs omp headlessly from the target's registered project directory.

use crate::discover;
use crate::protocol::{ProtocolError, ProtocolStore, TerminalStatus};
use crate::store;
use std::collections::HashMap;
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, ExitStatus};
use tinyjson::JsonValue;

pub(crate) const DEFAULT_NUDGE: &str = "You have new session-relay mail. Use the session-relay skill: call inbox to read your pending messages and act on them.";
const BOOL_FLAGS: [&str; 4] = ["dry", "json", "once", "all"];

fn die(msg: &str) -> ! {
    eprintln!("{msg}");
    std::process::exit(1);
}

fn protocol_die(error: &ProtocolError) -> ! {
    let status = if matches!(error, ProtocolError::CorrelationConflict) {
        2
    } else {
        1
    };
    eprintln!("{}", error.code());
    std::process::exit(status);
}

fn protocol_identity(identity: &str) -> store::Entry {
    store::resolve(identity).unwrap_or_else(|| {
        protocol_die(&ProtocolError::ProtocolStoreError(format!(
            "protocol identity is not registered: {identity}"
        )))
    })
}

// The registered current-project self identity — the cwd marker written by the
// SessionStart hook, resolved against the registry. Same dir-marker fallback
// the MCP bus uses when a `request`/`reply` tool call omits `from`.
fn protocol_self_identity() -> store::Entry {
    let cwd = cwd_string();
    let Some(id) = store::id_for_dir(&cwd) else {
        protocol_die(&ProtocolError::ProtocolStoreError(format!(
            "protocol identity is not registered: no session marker for {cwd}"
        )));
    };
    protocol_identity(&id)
}

pub(crate) struct Args(pub(crate) Vec<String>);

impl Args {
    fn sep_end(&self) -> usize {
        self.0
            .iter()
            .position(|arg| arg == "--")
            .unwrap_or(self.0.len())
    }
    // --name <value>; an empty value counts as absent (Node truthiness parity).
    pub(crate) fn flag(&self, name: &str) -> Option<&str> {
        let key = format!("--{name}");
        let args = &self.0[..self.sep_end()];
        let i = args.iter().position(|a| *a == key)?;
        args.get(i + 1)
            .map(String::as_str)
            .filter(|v| !v.is_empty())
    }
    pub(crate) fn has(&self, name: &str) -> bool {
        let key = format!("--{name}");
        self.0[..self.sep_end()].iter().any(|a| a == &key)
    }
    // Boolean flag present before the `--` separator; text after the separator
    // is verbatim message body and never parsed as options.
    pub(crate) fn has_before_sep(&self, name: &str) -> bool {
        let key = format!("--{name}");
        self.0
            .iter()
            .take_while(|arg| arg.as_str() != "--")
            .any(|arg| *arg == key)
    }
    pub(crate) fn unique_flag(&self, name: &str) -> Result<Option<&str>, String> {
        let key = format!("--{name}");
        let end = self.sep_end();
        let mut positions = self.0[..end]
            .iter()
            .enumerate()
            .filter_map(|(index, value)| (value == &key).then_some(index));
        let Some(index) = positions.next() else {
            return Ok(None);
        };
        if positions.next().is_some() {
            return Err(format!("duplicate --{name}"));
        }
        self.0[..end]
            .get(index + 1)
            .map(String::as_str)
            .filter(|value| !value.is_empty() && !value.starts_with("--"))
            .map(Some)
            .ok_or_else(|| format!("--{name} requires a value"))
    }
    pub(crate) fn hold_seconds(&self) -> Option<u64> {
        self.has("hold").then(|| {
            self.flag("hold")
                .and_then(|value| value.parse::<u64>().ok())
                .filter(|seconds| *seconds > 0)
                .unwrap_or(30)
        })
    }
    // positional args excluding flags + their values; a bare `--` ends option parsing.
    pub(crate) fn positionals(&self, from: usize) -> Vec<&str> {
        let mut out = Vec::new();
        let mut i = from;
        let end = self.sep_end();
        while i < end {
            let a = &self.0[i];
            if let Some(name) = a.strip_prefix("--") {
                let takes_value = if name == "hold" {
                    self.0[..end]
                        .get(i + 1)
                        .and_then(|value| value.parse::<u64>().ok())
                        .is_some_and(|seconds| seconds > 0)
                } else {
                    !BOOL_FLAGS.contains(&name)
                };
                if takes_value && i + 1 < end {
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
        tool: args.flag("tool").unwrap_or("omp").to_string(),
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

const ATTACH_WARNING: &str = "WARNING: split-brain risk — external omp processes do not share relay's resume lock. Attach only when the session is idle; relay doctor --id <id> shows watcher/lock state.";

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
    if target.tool != "omp" {
        eprintln!("{ATTACH_WARNING}");
        die(&format!(
            "attach target tool must be omp, got: {}",
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

    let dir = target
        .dir
        .as_deref()
        .filter(|dir| std::path::Path::new(dir).is_dir())
        .unwrap_or_else(|| die("attach refused: stored dir does not exist"));
    eprintln!("{ATTACH_WARNING}");
    let cmd = wake_cmd();
    if parsed.execute {
        eprintln!("--exec is deprecated; attach now retains a guarded parent until child exit");
    }
    let guard = match store::acquire_resume_lock(&target.id, "omp") {
        Ok(guard) => guard,
        Err(store::LockAcquireError::Busy(_)) => {
            eprintln!(
                "attach refused: relay wake is in flight for {} (resume lock held)",
                target.name.as_deref().unwrap_or(&target.id)
            );
            std::process::exit(3);
        }
        Err(store::LockAcquireError::Io(error)) => {
            eprintln!("attach refused: cannot acquire resume lock: {error}");
            std::process::exit(4);
        }
    };
    let status = Command::new(cmd)
        .args(["--resume", &target.id])
        .current_dir(dir)
        .status()
        .unwrap_or_else(|error| die(&format!("cannot launch omp: {error}")));
    drop(guard);
    std::process::exit(child_exit_code(status));
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
    let seconds = ms.max(0).unsigned_abs() / 1000;
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
    let rearm = format!(
        "{} watch --follow {} --tool omp",
        shell_quote(&relay_exe),
        shell_quote(&id)
    );
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

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn wake_cmd() -> String {
    std::env::var("RELAY_WAKE_CMD_OMP")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "omp".to_string())
}

pub(crate) fn doorbell_args(
    id: &str,
    message: &str,
    model: Option<&str>,
    effort: Option<&str>,
) -> (String, Vec<String>) {
    let mut args = vec![
        "-p".into(),
        "--resume".into(),
        id.into(),
        "--mode".into(),
        "json".into(),
    ];
    if let Some(model) = model {
        args.extend(["--model".into(), model.into()]);
    }
    if let Some(effort) = effort {
        args.extend(["--thinking".into(), effort.into()]);
    }
    args.extend(["--".into(), message.into()]);
    (wake_cmd(), args)
}

fn child_exit_code(status: ExitStatus) -> i32 {
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(1))
}

fn wake_dry_json(tool: &str, cmd: &str, args: &[String], dir: &str) -> JsonValue {
    let mut m: HashMap<String, JsonValue> = HashMap::new();
    m.insert("tool".into(), JsonValue::from(tool.to_string()));
    m.insert(
        "cmd".into(),
        JsonValue::from(format!("{cmd} {}", args.join(" "))),
    );
    m.insert(
        "args".into(),
        JsonValue::from(
            args.iter()
                .map(|a| JsonValue::from(a.to_string()))
                .collect::<Vec<_>>(),
        ),
    );
    m.insert("cwd".into(), JsonValue::from(dir.to_string()));
    JsonValue::from(m)
}

#[cfg(test)]
fn obj(v: &JsonValue) -> Option<&HashMap<String, JsonValue>> {
    v.get::<HashMap<String, JsonValue>>()
}

#[cfg(test)]
fn str_field<'a>(o: &'a HashMap<String, JsonValue>, k: &str) -> Option<&'a str> {
    o.get(k)?.get::<String>().map(String::as_str)
}

pub fn run(cmd: &str, raw: Vec<String>) -> ! {
    let args = Args(raw);
    if let Some(tool) = args.flag("tool") {
        if tool != "omp" {
            die(&format!("--tool must be omp, got: {tool}"));
        }
    }
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
                let o = r
                    .get::<HashMap<String, JsonValue>>()
                    .unwrap_or_else(|| die("row object"));
                let s = |k: &str| o.get(k).and_then(|v| v.get::<String>().cloned());
                let age = o
                    .get("ageSec")
                    .and_then(|v| v.get::<f64>().copied())
                    .unwrap_or(0.0);
                let age = if age.is_nan() { 0.0 } else { age };
                let age = age.clamp(i64::MIN as f64, i64::MAX as f64);
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "value is clamped to the target range above"
                )]
                let age = age as i64;
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
                die("usage: relay register <name> --id <uuid> [--dir <path>] [--tool omp]");
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
        "request" => {
            let rest = args.positionals(1);
            let body = args
                .message_after_sep()
                .unwrap_or_else(|| rest.iter().skip(1).copied().collect::<Vec<_>>().join(" "));
            let from = args.unique_flag("from").unwrap_or_else(|error| die(&error));
            let (Some(to), false) = (rest.first().copied(), body.is_empty()) else {
                die("usage: relay request <to> [--from <registered>] [--json] [--] <message...>");
            };
            let requester = match from {
                Some(identity) => protocol_identity(identity),
                None => protocol_self_identity(),
            };
            let responder = protocol_identity(to);
            let protocol = ProtocolStore::new(store::home_dir());
            let message = protocol
                .request(&requester.id, &responder.id, &body)
                .unwrap_or_else(|error| protocol_die(&error));
            if args.has_before_sep("json") {
                // Documented machine mode: the complete canonical MessageV2 envelope.
                println!(
                    "{}",
                    String::from_utf8(message.canonical_bytes())
                        .unwrap_or_else(|_| die("canonical MessageV2 is UTF-8"))
                );
            } else {
                println!(
                    r#"{{"correlation_id":"{}","message_id":"{}","outcome":"enqueued"}}"#,
                    message.correlation_id, message.id
                );
            }
            std::process::exit(0);
        }
        "reply" => {
            let rest = args.positionals(1);
            let body = args
                .message_after_sep()
                .unwrap_or_else(|| rest.iter().skip(1).copied().collect::<Vec<_>>().join(" "));
            let from = args.unique_flag("from").unwrap_or_else(|error| die(&error));
            let status = args
                .unique_flag("status")
                .unwrap_or_else(|error| die(&error));
            let (Some(correlation_id), Some(status), false) =
                (rest.first().copied(), status, body.is_empty())
            else {
                die(
                    "usage: relay reply <correlation-id> [--from <registered>] --status completed|failed [--] <message...>",
                );
            };
            let status = TerminalStatus::parse(status)
                .unwrap_or_else(|_| die("--status must be completed or failed"));
            let responder = match from {
                Some(identity) => protocol_identity(identity),
                None => protocol_self_identity(),
            };
            let protocol = ProtocolStore::new(store::home_dir());
            let outcome = protocol
                .reply(correlation_id, &responder.id, status, &body)
                .unwrap_or_else(|error| protocol_die(&error));
            println!(
                r#"{{"correlation_id":"{}","message_id":"{}","outcome":"enqueued","status":"{}"}}"#,
                outcome.message.correlation_id,
                outcome.message.id,
                status.as_str()
            );
            std::process::exit(0);
        }
        "ack" | "rollback" => {
            let pos = args.positionals(1);
            if pos.len() != 1 {
                die(&format!("usage: relay {cmd} <token>"));
            }
            let result = if cmd == "ack" {
                store::ack_hold(pos[0])
            } else {
                store::rollback_hold(pos[0])
            };
            result.unwrap_or_else(|error| die(&error.to_string()));
            std::process::exit(0);
        }
        "inbox" => {
            let pos = args.positionals(1);
            let Some(who) = pos.first() else {
                die("usage: relay inbox [--hold [<seconds>]] <nameOrId>");
            };
            let Some(target) = store::resolve(who) else {
                die(&format!("unknown session: {who}"));
            };
            if let Some(seconds) = args.hold_seconds() {
                let receipt = store::hold_mailbox(&target.id, "inbox", seconds)
                    .unwrap_or_else(|error| die(&error));
                let mut out: HashMap<String, JsonValue> = HashMap::new();
                out.insert(
                    "token".into(),
                    receipt
                        .token
                        .map(JsonValue::from)
                        .unwrap_or(JsonValue::Null),
                );
                out.insert(
                    "expires_at".into(),
                    receipt
                        .expires_at
                        .map(JsonValue::from)
                        .unwrap_or(JsonValue::Null),
                );
                out.insert("count".into(), JsonValue::from(receipt.count as f64));
                out.insert("messages".into(), JsonValue::from(receipt.messages));
                println!(
                    "{}",
                    JsonValue::from(out)
                        .stringify()
                        .unwrap_or_else(|error| die(&error.to_string()))
                );
                std::process::exit(0);
            }
            let msgs = store::drain_mailbox(&target.id)
                .unwrap_or_else(|error| die(&error))
                .into_messages();
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
            let custom_message = {
                let m = args.message_after_sep().unwrap_or_else(|| {
                    if explicit.is_some() {
                        rest.join(" ")
                    } else {
                        rest.iter().skip(1).copied().collect::<Vec<_>>().join(" ")
                    }
                });
                (!m.is_empty()).then_some(m)
            };
            let message = custom_message
                .clone()
                .unwrap_or_else(|| DEFAULT_NUDGE.to_string());
            let target = explicit.or_else(|| {
                rest.first()
                    .and_then(|who| store::resolve(who))
                    .map(from_entry)
            });
            let Some(target) = target else {
                die(
                    "usage: relay wake <nameOrId> [--model <m>] [--effort <e>] [message...] | wake --id <id> --dir <cwd> --tool omp [--model <m>] [--effort <e>] [message...]",
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
            let model = args.flag("model");
            let effort = args.flag("effort");
            if model.is_none() {
                eprintln!(
                    "[relay wake] no --model given — pass --model/--effort to pin a deliberate doorbell model"
                );
            }
            let (cmd, cargs) = doorbell_args(&target.id, &message, model, effort);
            if args.has("dry") {
                println!(
                    "{}",
                    wake_dry_json(&target.tool, &cmd, &cargs, &dir)
                        .stringify()
                        .unwrap_or_else(|_| "{}".into())
                );
                std::process::exit(0);
            }
            // Refuse stale registrations rather than resuming from another directory.
            if !std::path::Path::new(&dir).is_dir() {
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
            if store::resolve(&target.id).is_none() {
                store::register(&target.id, Some(&dir), None, Some(&target.tool)).unwrap_or_else(
                    |error| die(&format!("cannot register explicit wake target: {error}")),
                );
            }
            let status = Command::new(cmd)
                .args(cargs)
                .current_dir(&dir)
                .status()
                .unwrap_or_else(|error| die(&format!("cannot launch omp: {error}")));
            drop(_resume_guard);
            std::process::exit(child_exit_code(status));
        }
        _ => die(
            "usage: relay discover [--within min] [--tool omp] | list | register <name> --id <uuid> [--dir <path>] | send <to> <msg> | request <to> [--from <registered>] [--json] [--] <msg> | reply <correlation-id> [--from <registered>] --status completed|failed [--] <msg> | inbox <who> | peek <who> | attach <who> [--exec] | wake <who> [--model m] [--effort e] [msg] | doctor [--id <session>]",
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
    fn hold_optional_seconds_preserve_target_and_separator() {
        for (argv, seconds, targets) in [
            (vec!["inbox", "--hold", "target"], Some(30), vec!["target"]),
            (
                vec!["inbox", "--hold", "7", "target"],
                Some(7),
                vec!["target"],
            ),
            (
                vec!["inbox", "target", "--", "--hold", "7"],
                None,
                vec!["target"],
            ),
            (
                vec!["inbox", "target", "--hold", "--", "7"],
                Some(30),
                vec!["target"],
            ),
        ] {
            let args = Args(strings(&argv));
            assert_eq!(args.hold_seconds(), seconds);
            assert_eq!(args.positionals(1), targets);
        }
    }

    #[test]
    fn send_separator_keeps_flag_shaped_message_opaque() {
        let args = Args(strings(&["send", "--from", "A", "B", "--", "--id", "X"]));
        assert_eq!(args.message_after_sep().as_deref(), Some("--id X"));
        assert_eq!(args.flag("id"), None);
        assert!(!args.has("id"));
        assert_eq!(args.positionals(1), vec!["B"]);
        assert!(args.positionals(5).is_empty());
    }

    #[test]
    fn wake_separator_keeps_explicit_tool() {
        let args = Args(strings(&[
            "wake", "--id", "A", "--dir", ".", "--tool", "omp", "--", "--tool", "other",
        ]));
        assert_eq!(args.flag("tool"), Some("omp"));
        assert_eq!(args.unique_flag("tool"), Ok(Some("omp")));
    }

    #[test]
    fn separator_cannot_supply_a_flag_value() {
        let args = Args(strings(&["send", "B", "--from", "--", "A"]));
        assert_eq!(args.flag("from"), None);
        assert_eq!(
            args.unique_flag("from"),
            Err("--from requires a value".to_string())
        );
        assert_eq!(args.positionals(1), vec!["B"]);
        assert_eq!(args.message_after_sep().as_deref(), Some("A"));
    }

    #[test]
    fn omp_wake_argv_uses_json_mode_and_thinking_before_message() {
        let (cmd, args) = doorbell_args("u-1", "--tool other", Some("model-name"), Some("high"));
        assert_eq!(cmd, "omp");
        assert_eq!(
            args,
            strings(&[
                "-p",
                "--resume",
                "u-1",
                "--mode",
                "json",
                "--model",
                "model-name",
                "--thinking",
                "high",
                "--",
                "--tool other",
            ])
        );
    }

    #[test]
    fn omp_wake_dry_json_includes_full_command_and_preserves_argv() {
        let (cmd, args) = doorbell_args("u-1", "ping", None, None);
        let output = wake_dry_json("omp", &cmd, &args, ".");
        let output = output.stringify().unwrap().parse::<JsonValue>().unwrap();
        let output = obj(&output).unwrap();
        assert_eq!(str_field(output, "tool"), Some("omp"));
        assert_eq!(
            str_field(output, "cmd"),
            Some("omp -p --resume u-1 --mode json -- ping")
        );
        assert_eq!(
            output.get("args"),
            Some(&JsonValue::from(
                strings(&["-p", "--resume", "u-1", "--mode", "json", "--", "ping"])
                    .into_iter()
                    .map(JsonValue::from)
                    .collect::<Vec<_>>()
            ))
        );
        assert_eq!(str_field(output, "cwd"), Some("."));
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

    #[test]
    fn request_json_flag_counts_only_before_the_separator() {
        let args = Args(strings(&["request", "agent-b", "--json", "--", "body"]));
        assert!(args.has_before_sep("json"));

        let args = Args(strings(&[
            "request",
            "agent-b",
            "--",
            "pass --json literally",
        ]));
        assert!(!args.has_before_sep("json"));
    }
}
