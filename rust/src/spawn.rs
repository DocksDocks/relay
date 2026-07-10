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
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

const DEFAULT_TIMEOUT_SECS: u64 = 30;
const BIRTH_POLL_MS: u64 = 250;
const SPAWN_LOG_MAX_BYTES: u64 = 4 * 1024 * 1024;
const SPAWN_LOG_RETAIN_BYTES: u64 = 3 * 1024 * 1024;
const SPAWN_LOG_BUFFER_BYTES: usize = 64 * 1024;

fn die(msg: &str) -> ! {
    eprintln!("{msg}");
    std::process::exit(1);
}

const USAGE: &str = "usage: relay spawn <dir> [--tool claude|codex] [--model <m>] [--effort <e>] [--name <busName>] [--reply-to <nameOrId>] [--timeout <sec>] [--read-only] [--full-access] [--watch] [--dry] [--] <first task>";

fn exit_code(status: &ExitStatus) -> i32 {
    status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(1)
}

fn elapsed(started: Instant) -> String {
    let elapsed = started.elapsed();
    if elapsed < Duration::from_secs(1) {
        format!("{}ms", elapsed.as_millis())
    } else {
        format!("{:.1}s", elapsed.as_secs_f64())
    }
}

fn compact_spawn_log(file: &mut fs::File) -> Result<(), String> {
    let len = file
        .seek(SeekFrom::End(0))
        .map_err(|e| format!("seek spawn log: {e}"))?;
    if len <= SPAWN_LOG_MAX_BYTES {
        return Ok(());
    }
    let retain = len.min(SPAWN_LOG_RETAIN_BYTES);
    file.seek(SeekFrom::End(-(retain as i64)))
        .map_err(|e| format!("seek spawn log tail: {e}"))?;
    let mut tail = vec![0_u8; retain as usize];
    file.read_exact(&mut tail)
        .map_err(|e| format!("read spawn log tail: {e}"))?;
    file.seek(SeekFrom::Start(0))
        .and_then(|_| file.write_all(&tail))
        .and_then(|()| file.set_len(retain))
        .and_then(|()| file.seek(SeekFrom::End(0)).map(|_| ()))
        .map_err(|e| format!("compact spawn log: {e}"))
}

/// Hidden stderr pump used by `relay spawn`. It is a separate process so the
/// pipe keeps draining after the parent relay returns at birth registration.
pub fn run_log_writer(id: &str) -> ! {
    if !store::is_uuid(id) {
        die("spawn log writer requires a UUID");
    }
    let _pump_liveness = store::acquire_spawn_pump_lock()
        .unwrap_or_else(|e| die(&format!("acquire spawn-pump liveness lock: {e}")));
    let dir = store::home_dir().join("spawn-logs");
    fs::create_dir_all(&dir).unwrap_or_else(|e| die(&format!("create spawn-log dir: {e}")));
    let path = dir.join(format!("{}.stderr", store::sanitize(id)));
    let mut file = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .unwrap_or_else(|e| die(&format!("open spawn log {}: {e}", path.display())));
    file.seek(SeekFrom::End(0))
        .unwrap_or_else(|e| die(&format!("seek spawn log {}: {e}", path.display())));

    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut buffer = [0_u8; SPAWN_LOG_BUFFER_BYTES];
    loop {
        let read = input
            .read(&mut buffer)
            .unwrap_or_else(|e| die(&format!("read spawn stderr: {e}")));
        if read == 0 {
            break;
        }
        file.write_all(&buffer[..read])
            .unwrap_or_else(|e| die(&format!("write spawn log {}: {e}", path.display())));
        compact_spawn_log(&mut file).unwrap_or_else(|e| die(&e));
    }
    file.flush()
        .unwrap_or_else(|e| die(&format!("flush spawn log {}: {e}", path.display())));
    std::process::exit(0);
}

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

fn append_model_effort_args(
    a: &mut Vec<String>,
    tool: &str,
    model: Option<&str>,
    effort: Option<&str>,
) {
    if let Some(model) = model {
        a.push(if tool == "codex" { "-m" } else { "--model" }.into());
        a.push(model.into());
    }
    if let Some(effort) = effort {
        if tool == "codex" {
            a.push("-c".into());
            a.push(format!("model_reasoning_effort={effort}"));
        } else {
            a.push("--effort".into());
            a.push(effort.into());
        }
    }
}

// Availability probe behind the codex-first default: true when the codex
// command (or its RELAY_SPAWN_CMD_CODEX override) resolves to an executable.
fn is_executable(p: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

fn codex_available() -> bool {
    let cmd = child_cmd("codex");
    if cmd.contains('/') {
        return is_executable(std::path::Path::new(&cmd));
    }
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| is_executable(&d.join(&cmd))))
        .unwrap_or(false)
}

// Tool resolution: --tool flag > RELAY_SPAWN_TOOL env > codex when its CLI is
// available > claude. Either fallback prints a note naming the choice.
fn resolve_spawn_tool(
    explicit: Option<&str>,
    env_default: Option<String>,
    codex_present: bool,
) -> Result<(String, Option<&'static str>), String> {
    match explicit {
        Some(t @ ("claude" | "codex")) => Ok((t.to_string(), None)),
        Some(other) => Err(format!(
            "unknown --tool: {other} (valid values: claude|codex)"
        )),
        None => match env_default.as_deref() {
            Some(t @ ("claude" | "codex")) => Ok((t.to_string(), None)),
            Some(other) => Err(format!(
                "unknown RELAY_SPAWN_TOOL: {other} (valid values: claude|codex)"
            )),
            None if codex_present => Ok((
                "codex".to_string(),
                Some("[relay spawn] no --tool given — codex available, defaulting to codex"),
            )),
            None => Ok((
                "claude".to_string(),
                Some("[relay spawn] no --tool given — codex not found, defaulting to claude"),
            )),
        },
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
    model: Option<&str>,
    effort: Option<&str>,
    prompt: &str,
) -> Vec<String> {
    let mut a: Vec<String> = Vec::new();
    if tool == "codex" {
        a.push("exec".into());
        a.extend(perm.iter().cloned());
        append_model_effort_args(&mut a, tool, model, effort);
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
        append_model_effort_args(&mut a, tool, model, effort);
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

    let (tool, default_note) = resolve_spawn_tool(
        args.flag("tool"),
        std::env::var("RELAY_SPAWN_TOOL").ok(),
        codex_available(),
    )
    .unwrap_or_else(|e| die(&e));
    if let Some(note) = default_note {
        eprintln!("{note}");
    }
    let model = args.flag("model");
    let effort = args.flag("effort");
    if model.is_none() {
        eprintln!(
            "[relay spawn] no --model given — pass --model/--effort to pin a deliberate worker model"
        );
    }
    let read_only = args.has("read-only");
    let full_access = args.has("full-access");
    let watch = args.has("watch");
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
    let cargs = child_args(
        &tool,
        &perm,
        premint.as_deref(),
        skip_git_check,
        model,
        effort,
        &prompt,
    );

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
    let log_id = premint.clone().unwrap_or_else(store::uuid_v4);
    let log_path = log_dir.join(format!("{log_id}.stderr"));
    // Synchronous create/truncate means this spawn can never inherit stale
    // content, even if the pump process is delayed before opening the path.
    fs::File::create(&log_path).unwrap_or_else(|e| {
        die(&format!(
            "cannot create spawn log {}: {e}",
            log_path.display()
        ))
    });
    let relay_exe = std::env::current_exe().unwrap_or_else(|_| "relay".into());
    let mut log_writer = Command::new(relay_exe)
        .arg("__spawn-log-writer")
        .arg(&log_id)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|e| die(&format!("cannot launch spawn log writer: {e}")));
    let log_stdin = log_writer
        .stdin
        .take()
        .unwrap_or_else(|| die("spawn log writer stdin unavailable"));

    let marker_before = store::id_for_dir(&dir_s);
    let mut command = Command::new(&cmd);
    command
        .args(&cargs)
        .current_dir(&dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log_stdin))
        .process_group(0);
    let launched_at = Instant::now();
    let mut child = command
        .spawn()
        .unwrap_or_else(|e| die(&format!("failed to launch {cmd}: {e}")));
    drop(command); // close the parent's duplicate stderr-pipe writer

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
        match child.try_wait() {
            Ok(Some(status)) => {
                let _ = log_writer.wait();
                eprintln!(
                    "child exited before birth registration (status {}) — see {}",
                    exit_code(&status),
                    log_path.display()
                );
                std::process::exit(exit_code(&status).max(1));
            }
            Ok(None) => {}
            Err(e) => die(&format!("cannot inspect child status: {e}")),
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
    if id != log_id {
        let born_path = log_dir.join(format!("{id}.stderr"));
        match fs::rename(&log_path, &born_path) {
            Ok(()) => {}
            Err(e) => {
                eprintln!("[relay spawn] cannot associate spawn log with born session {id}: {e}")
            }
        }
    }
    let display = if let Some(name) = args.flag("name") {
        store::register(&id, Some(&dir_s), Some(name), Some(&tool)).unwrap_or_else(|e| die(&e));
        name.to_string()
    } else {
        id.clone()
    };
    if !watch {
        if let Some(name) = args.flag("name") {
            println!("spawned {name} ({id}) in {dir_s}");
        } else {
            println!("spawned {id} in {dir_s}");
        }
        std::process::exit(0);
    }
    let status = child
        .wait()
        .unwrap_or_else(|e| die(&format!("cannot wait for child: {e}")));
    let _ = log_writer.wait();
    let duration = elapsed(launched_at);
    if status.success() {
        println!("spawned {display}; first turn complete; {duration}");
        std::process::exit(0);
    }
    let code = exit_code(&status);
    println!("spawned {display}; first turn failed (exit {code}); {duration}");
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_log_compaction_keeps_the_newest_tail() {
        let path = std::env::temp_dir().join(format!("relay-spawn-log-{}", store::uuid_v4()));
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path)
            .expect("create spawn-log fixture");
        file.write_all(&vec![b'x'; SPAWN_LOG_MAX_BYTES as usize])
            .expect("write prefix");
        file.write_all(b"newest-tail-marker").expect("write marker");
        compact_spawn_log(&mut file).expect("compact fixture");
        assert_eq!(
            file.metadata().expect("stat fixture").len(),
            SPAWN_LOG_RETAIN_BYTES
        );
        file.seek(SeekFrom::End(-18)).expect("seek marker");
        let mut marker = String::new();
        file.read_to_string(&mut marker).expect("read marker");
        assert_eq!(marker, "newest-tail-marker");
        drop(file);
        fs::remove_file(path).expect("remove fixture");
    }

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
        let a = child_args(
            "claude",
            &perm,
            Some("u-1"),
            false,
            None,
            None,
            "-rf looks like a flag",
        );
        assert_eq!(
            a,
            [
                "-p",
                "--session-id",
                "u-1",
                "--permission-mode",
                "auto",
                "--",
                "-rf looks like a flag"
            ]
        );
        assert!(a.contains(&"--permission-mode".to_string()));
        assert!(!a.iter().any(|x| x.contains("output-format")));
        let sep = a.iter().position(|x| x == "--").unwrap();
        assert_eq!(a[sep + 1], "-rf looks like a flag"); // fenced, final positional
        assert_eq!(a.len(), sep + 2);
    }

    #[test]
    fn codex_argv_adds_git_bypass_only_when_asked() {
        let perm = perm_args("codex", false, false);
        let with = child_args("codex", &perm, None, true, None, None, "t");
        assert_eq!(with[0], "exec");
        assert!(with.contains(&"--skip-git-repo-check".to_string()));
        let without = child_args("codex", &perm, None, false, None, None, "t");
        assert!(!without.contains(&"--skip-git-repo-check".to_string()));
        assert!(!without.iter().any(|x| x == "--json"));
    }

    #[test]
    fn child_argv_maps_model_and_effort_per_tool_before_prompt_fence() {
        let claude_perm = perm_args("claude", false, false);
        let claude = child_args(
            "claude",
            &claude_perm,
            Some("u-1"),
            false,
            Some("opus"),
            Some("max"),
            "t",
        );
        assert_eq!(
            claude,
            [
                "-p",
                "--session-id",
                "u-1",
                "--permission-mode",
                "auto",
                "--model",
                "opus",
                "--effort",
                "max",
                "--",
                "t"
            ]
        );

        let codex_perm = perm_args("codex", false, false);
        let codex = child_args(
            "codex",
            &codex_perm,
            None,
            true,
            Some("gpt-5.5"),
            Some("xhigh"),
            "t",
        );
        assert_eq!(
            codex,
            [
                "exec",
                "--sandbox",
                "workspace-write",
                "-m",
                "gpt-5.5",
                "-c",
                "model_reasoning_effort=xhigh",
                "--skip-git-repo-check",
                "--",
                "t"
            ]
        );
    }

    #[test]
    fn spawn_tool_resolution_prefers_flag_then_env_then_availability() {
        assert_eq!(
            resolve_spawn_tool(Some("claude"), Some("codex".to_string()), true).unwrap(),
            ("claude".to_string(), None)
        );
        assert_eq!(
            resolve_spawn_tool(None, Some("claude".to_string()), true).unwrap(),
            ("claude".to_string(), None)
        );
        let (tool, note) = resolve_spawn_tool(None, None, true).unwrap();
        assert_eq!(tool, "codex");
        assert!(note.unwrap().contains("defaulting to codex"));
        let (tool, note) = resolve_spawn_tool(None, None, false).unwrap();
        assert_eq!(tool, "claude");
        assert!(note.unwrap().contains("defaulting to claude"));
    }

    #[test]
    fn spawn_tool_resolution_rejects_invalid_flag_or_env() {
        assert!(
            resolve_spawn_tool(Some("zed"), None, true)
                .unwrap_err()
                .contains("valid values: claude|codex")
        );
        assert!(
            resolve_spawn_tool(None, Some("zed".to_string()), false)
                .unwrap_err()
                .contains("RELAY_SPAWN_TOOL")
        );
    }
}
