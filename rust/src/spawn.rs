// spawn.rs — `relay spawn <dir> … -- <task>`: birth a NEW persistent agent
// session (Claude or Codex) in any project dir and hand it to the bus.
// Live-verified 2026-07-02 (claude 2.1.198, codex-cli 0.142.5):
//   - headless `claude -p` and `codex exec` BOTH fire the SessionStart hook,
//     so the child self-registers on the bus at birth;
//   - `claude -p --session-id <uuid>` accepts a pre-minted id (birth = watch
//     for that exact id); `codex exec` has no pre-set-id flag (birth = watch
//     the dir marker for a NEW id);
//   - a non-git dir needs `--skip-git-repo-check` on the codex argv.
// The child launches DETACHED (own process group, null stdin/stdout, stderr
// to a spawn-log so a fast-failing child stays diagnosable) — spawn returns
// as soon as birth is confirmed, long before the first task finishes; the
// conversation continues over the bus (`send`/`inbox`) + `relay wake`.
// Permission posture (user decision, native picker 2026-07-02 — symmetric):
//   default  claude `--permission-mode auto`   codex `--sandbox workspace-write`
//   --read-only     `--permission-mode plan`         `--sandbox read-only`
//   --full-access   `--permission-mode bypassPermissions`  `--sandbox danger-full-access`
// Guardrail rules ride in the first prompt on EVERY spawn regardless of flag.
// The first prompt is TRUSTED (parent instructing its own child) — it is NOT
// wrapped in the untrusted mail fence; later bus mail to the child is.

use crate::cli::Args;
use crate::store;
use std::fs;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const DEFAULT_TIMEOUT_SECS: u64 = 30;
const BIRTH_POLL_MS: u64 = 250;

fn die(msg: &str) -> ! {
    eprintln!("{msg}");
    std::process::exit(1);
}

const USAGE: &str = "usage: relay spawn <dir> [--tool claude|codex] [--name <busName>] [--reply-to <nameOrId>] [--timeout <sec>] [--read-only] [--full-access] [--dry] [--] <first task>";

// Symmetric per-tool permission mapping. NEVER a --dangerously-* variant.
fn perm_args(tool: &str, read_only: bool, full_access: bool) -> [String; 2] {
    if tool == "codex" {
        let mode = if read_only {
            "read-only"
        } else if full_access {
            "danger-full-access"
        } else {
            "workspace-write"
        };
        ["--sandbox".into(), mode.into()]
    } else {
        let mode = if read_only {
            "plan"
        } else if full_access {
            "bypassPermissions"
        } else {
            "auto"
        };
        ["--permission-mode".into(), mode.into()]
    }
}

// The child's first prompt: standing bus-worker prefix + verbatim guardrail
// rules + the task. `abs_relay` makes the reply loop work even in a project
// with no session-relay installed — spawn IS that binary. A pre-minted id
// (claude only) is baked in as `--from` so the reply stays attributed to the
// worker even when the shared-dir marker has moved on; codex workers learn
// theirs from the identity line the hook injects at session start.
fn build_prompt(reply_to: &str, abs_relay: &str, premint: Option<&str>, task: &str) -> String {
    let from = premint
        .map(|id| format!("--from {id} "))
        .unwrap_or_default();
    format!(
        r#"You are a session-relay bus worker spawned by "{reply_to}". You are running in a
fresh session in this project — its CLAUDE.md/AGENTS.md, skills, and plugins apply.
When you finish, or if you need a decision, report to "{reply_to}" over the bus.
PRIMARY (works even if session-relay isn't installed in this project) — run:
  {abs_relay} send "{reply_to}" {from}-- "<your message>"
(that is the absolute path to the relay binary that spawned you). If this project has
session-relay installed, the session-relay skill's send tool works too.

Guardrail rules (non-negotiable):
1. Always create and work on a separate git branch; never commit directly to the
   default branch.
2. Never modify live/production systems (e.g. over ssh). Read-only probes are
   allowed; mutations are not.
3. Destructive or irreversible operations require asking "{reply_to}" over the bus
   first and waiting for approval.

Your task:

{task}"#
    )
}

// Child argv (after the command itself). The prompt always sits behind a `--`
// fence so a dash-leading task can never be parsed as a flag. No output-format
// flags: the child is detached with null stdout — nothing ever reads it.
fn child_args(
    tool: &str,
    perm: &[String; 2],
    premint: Option<&str>,
    skip_git_check: bool,
    prompt: &str,
) -> Vec<String> {
    let mut a: Vec<String> = Vec::new();
    if tool == "codex" {
        a.push("exec".into());
        a.extend(perm.iter().cloned());
        if skip_git_check {
            a.push("--skip-git-repo-check".into());
        }
    } else {
        a.push("-p".into());
        if let Some(id) = premint {
            a.push("--session-id".into());
            a.push(id.into());
        }
        a.extend(perm.iter().cloned());
    }
    a.push("--".into());
    a.push(prompt.into());
    a
}

fn child_cmd(tool: &str) -> String {
    let var = if tool == "codex" {
        "RELAY_SPAWN_CMD_CODEX"
    } else {
        "RELAY_SPAWN_CMD_CLAUDE"
    };
    std::env::var(var)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| tool.to_string())
}

// Default --reply-to: the bus identity of the session spawn was invoked from
// (name when registered, raw id otherwise — an id is a valid send target).
fn default_reply_to() -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    let id = store::id_for_dir(&cwd.to_string_lossy())?;
    let name = store::resolve(&id).and_then(|e| e.name);
    Some(name.unwrap_or(id))
}

pub fn run(raw: Vec<String>) -> ! {
    let args = Args(raw);
    let pos = args.positionals(1);
    let Some(dir_raw) = pos.first() else {
        die(USAGE);
    };
    let dir = match fs::canonicalize(dir_raw) {
        Ok(d) if d.is_dir() => d,
        _ => die(&format!("target dir does not exist: {dir_raw}")),
    };
    let dir_s = dir.to_string_lossy().to_string();

    let task = args
        .message_after_sep()
        .or_else(|| {
            let rest = &pos[1..];
            (!rest.is_empty()).then(|| rest.join(" "))
        })
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| die(USAGE));

    let tool = match args.flag("tool") {
        Some(t @ ("claude" | "codex")) => t.to_string(),
        Some(other) => die(&format!("unknown --tool: {other} (claude|codex)")),
        None => {
            eprintln!("[relay spawn] no --tool given — defaulting to claude");
            "claude".to_string()
        }
    };
    let read_only = args.has("read-only");
    let full_access = args.has("full-access");
    if read_only && full_access {
        die("--read-only and --full-access are mutually exclusive");
    }
    let timeout_secs: u64 = args
        .flag("timeout")
        .map(|v| {
            v.parse()
                .unwrap_or_else(|_| die("--timeout must be a number of seconds"))
        })
        .unwrap_or(DEFAULT_TIMEOUT_SECS);

    let reply_to = args
        .flag("reply-to")
        .map(str::to_string)
        .or_else(default_reply_to)
        .unwrap_or_else(|| {
            die("no --reply-to given and this directory has no registered session; pass --reply-to <name-or-id> (see `relay whoami`)")
        });

    let abs_relay = std::env::current_exe()
        .ok()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| "relay".to_string());

    // Claude accepts a pre-minted id (watch for exactly it); codex has no
    // pre-set-id flag, so birth is detected by the dir marker changing.
    let premint = (tool != "codex").then(store::uuid_v4);
    let prompt = build_prompt(&reply_to, &abs_relay, premint.as_deref(), &task);
    let perm = perm_args(&tool, read_only, full_access);
    let skip_git_check = tool == "codex" && !dir.join(".git").exists();
    let cmd = child_cmd(&tool);
    let cargs = child_args(&tool, &perm, premint.as_deref(), skip_git_check, &prompt);

    if args.has("dry") {
        let mut m: std::collections::HashMap<String, tinyjson::JsonValue> =
            std::collections::HashMap::new();
        m.insert("tool".into(), tinyjson::JsonValue::from(tool.clone()));
        m.insert("cmd".into(), tinyjson::JsonValue::from(cmd.clone()));
        m.insert(
            "args".into(),
            tinyjson::JsonValue::from(
                cargs
                    .iter()
                    .map(|a| tinyjson::JsonValue::from(a.clone()))
                    .collect::<Vec<_>>(),
            ),
        );
        m.insert("cwd".into(), tinyjson::JsonValue::from(dir_s.clone()));
        m.insert("prompt".into(), tinyjson::JsonValue::from(prompt.clone()));
        println!(
            "{}",
            tinyjson::JsonValue::from(m).stringify().unwrap_or_default()
        );
        std::process::exit(0);
    }

    // stderr → a spawn-log so a child that execs then dies fast (bad flag,
    // auth failure) stays diagnosable; stdout/stdin are null by design.
    let log_dir = store::home_dir().join("spawn-logs");
    let _ = fs::create_dir_all(&log_dir);
    let log_path = log_dir.join(format!(
        "{}.stderr",
        premint.clone().unwrap_or_else(store::uuid_v4)
    ));
    let log_file = fs::File::create(&log_path).unwrap_or_else(|e| {
        die(&format!(
            "cannot create spawn log {}: {e}",
            log_path.display()
        ))
    });

    let marker_before = store::id_for_dir(&dir_s);
    let mut child = Command::new(&cmd);
    child
        .args(&cargs)
        .current_dir(&dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log_file))
        .process_group(0);
    match child.spawn() {
        Ok(_) => {}
        Err(e) => die(&format!("failed to launch {cmd}: {e}")),
    }

    // Birth watch: the child's SessionStart hook registers it on the bus.
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let born: Option<String> = loop {
        let hit = match &premint {
            Some(id) => store::resolve(id).map(|e| e.id),
            None => match store::id_for_dir(&dir_s) {
                Some(cur) if Some(&cur) != marker_before.as_ref() && store::is_uuid(&cur) => {
                    Some(cur)
                }
                _ => None,
            },
        };
        if hit.is_some() {
            break hit;
        }
        if Instant::now() > deadline {
            break None;
        }
        std::thread::sleep(Duration::from_millis(BIRTH_POLL_MS));
    };

    let Some(id) = born else {
        die(&format!(
            "no birth registration within {timeout_secs}s — the child may have failed early (see {}) or be slow to start (try `relay discover`)",
            log_path.display()
        ));
    };
    if let Some(name) = args.flag("name") {
        store::register(&id, Some(&dir_s), Some(name), Some(&tool)).unwrap_or_else(|e| die(&e));
        println!("spawned {name} ({id}) in {dir_s}");
    } else {
        println!("spawned {id} in {dir_s}");
    }
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perm_mapping_is_symmetric_across_tools() {
        assert_eq!(
            perm_args("claude", false, false),
            ["--permission-mode", "auto"]
        );
        assert_eq!(
            perm_args("claude", true, false),
            ["--permission-mode", "plan"]
        );
        assert_eq!(
            perm_args("claude", false, true),
            ["--permission-mode", "bypassPermissions"]
        );
        assert_eq!(
            perm_args("codex", false, false),
            ["--sandbox", "workspace-write"]
        );
        assert_eq!(perm_args("codex", true, false), ["--sandbox", "read-only"]);
        assert_eq!(
            perm_args("codex", false, true),
            ["--sandbox", "danger-full-access"]
        );
    }

    #[test]
    fn prompt_carries_reply_target_abs_relay_and_all_guardrails() {
        let p = build_prompt("boss", "/opt/bin/relay", None, "do the thing");
        assert!(p.contains(r#"report to "boss" over the bus"#));
        assert!(p.contains(r#"/opt/bin/relay send "boss" -- "#));
        assert!(!p.contains("--from"));
        assert!(p.contains("separate git branch"));
        assert!(p.contains("Never modify live/production systems"));
        assert!(p.contains("Destructive or irreversible operations require asking"));
        assert!(p.trim_end().ends_with("do the thing"));
    }

    #[test]
    fn prompt_bakes_the_preminted_id_into_the_reply_command_as_from() {
        let p = build_prompt("boss", "/opt/bin/relay", Some("u-7"), "t");
        assert!(p.contains(r#"/opt/bin/relay send "boss" --from u-7 -- "#));
    }

    #[test]
    fn claude_argv_premints_id_and_never_sets_output_format() {
        let perm = perm_args("claude", false, false);
        let a = child_args("claude", &perm, Some("u-1"), false, "-rf looks like a flag");
        assert_eq!(a[0], "-p");
        assert_eq!(&a[1..3], ["--session-id", "u-1"]);
        assert!(a.contains(&"--permission-mode".to_string()));
        assert!(!a.iter().any(|x| x.contains("output-format")));
        let sep = a.iter().position(|x| x == "--").unwrap();
        assert_eq!(a[sep + 1], "-rf looks like a flag"); // fenced, final positional
        assert_eq!(a.len(), sep + 2);
    }

    #[test]
    fn codex_argv_adds_git_bypass_only_when_asked() {
        let perm = perm_args("codex", false, false);
        let with = child_args("codex", &perm, None, true, "t");
        assert_eq!(with[0], "exec");
        assert!(with.contains(&"--skip-git-repo-check".to_string()));
        let without = child_args("codex", &perm, None, false, "t");
        assert!(!without.contains(&"--skip-git-repo-check".to_string()));
        assert!(!without.iter().any(|x| x == "--json"));
    }
}
