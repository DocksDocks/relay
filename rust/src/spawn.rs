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

use crate::appserver;
use crate::cli::Args;
use crate::fanout::{self, FanoutMode, FanoutState, FanoutStore};
use crate::lifecycle::{
    ChildLaunchSpec, ClaimManagedAttach, ClaimOutcome, ExecutionBackend, LifecycleStore,
    MANAGED_CANCEL_GRACE_MS, ManagedState, OperationKind, PendingAttachSpec, ReentryGuard,
    RequiredScope, TerminalAction,
};
use crate::store;
use rustix::fs::{FlockOperation, flock};
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::process::*;
use std::process::{Child, ChildStdin, Command, ExitStatus, Output, Stdio};
use std::time::{Duration, Instant};
use tinyjson::JsonValue;

const DEFAULT_TIMEOUT_SECS: u64 = 30;
const BIRTH_POLL_MS: u64 = 250;
const SPAWN_LOG_MAX_BYTES: u64 = 4 * 1024 * 1024;
const SPAWN_LOG_RETAIN_BYTES: u64 = 3 * 1024 * 1024;
const SPAWN_LOG_BUFFER_BYTES: usize = 64 * 1024;

struct ManagedBirth {
    worker_id: String,
    generation: String,
    raw_token: String,
}

struct FanoutSpawnConfig {
    reservation_id: String,
    cwd: String,
    tool: String,
    command: String,
    arguments: Vec<String>,
    preminted_session_id: Option<String>,
    name: Option<String>,
    timeout_secs: u64,
}

struct FanoutLaunchOptions<'a> {
    repository: &'a std::path::Path,
    mode: FanoutMode,
    parent_session_id: &'a str,
    tool: &'a str,
    model: Option<&'a str>,
    effort: Option<&'a str>,
    name: Option<&'a str>,
    reply_to: &'a str,
    timeout_secs: u64,
    permissions: &'a [String; 2],
    task: &'a str,
}

enum WorkerWorkspace<'a> {
    CreateBranch,
    AssignedFanoutWorktree { handback_session_id: &'a str },
}

impl FanoutSpawnConfig {
    fn to_json(&self) -> String {
        let mut object = HashMap::new();
        for (key, value) in [
            ("reservation_id", Some(self.reservation_id.as_str())),
            ("cwd", Some(self.cwd.as_str())),
            ("tool", Some(self.tool.as_str())),
            ("command", Some(self.command.as_str())),
            ("preminted_session_id", self.preminted_session_id.as_deref()),
            ("name", self.name.as_deref()),
        ] {
            object.insert(
                key.to_string(),
                value
                    .map(|value| JsonValue::from(value.to_string()))
                    .unwrap_or(JsonValue::from(())),
            );
        }
        object.insert(
            "arguments".into(),
            JsonValue::from(
                self.arguments
                    .iter()
                    .cloned()
                    .map(JsonValue::from)
                    .collect::<Vec<_>>(),
            ),
        );
        object.insert(
            "timeout_secs".into(),
            JsonValue::from(self.timeout_secs as f64),
        );
        JsonValue::from(object)
            .stringify()
            .unwrap_or_else(|_| "{}".to_string())
    }

    fn from_stdin() -> Result<Self, String> {
        let mut input = String::new();
        std::io::stdin()
            .read_to_string(&mut input)
            .map_err(|error| format!("read fanout supervisor config: {error}"))?;
        let object = input
            .parse::<JsonValue>()
            .ok()
            .and_then(|value| value.get::<HashMap<String, JsonValue>>().cloned())
            .ok_or_else(|| "invalid fanout supervisor config".to_string())?;
        let required = |key: &str| {
            config_string(&object, key)
                .ok_or_else(|| format!("fanout supervisor config missing {key}"))
        };
        let arguments = object
            .get("arguments")
            .and_then(|value| value.get::<Vec<JsonValue>>())
            .ok_or_else(|| "fanout supervisor config missing arguments".to_string())?
            .iter()
            .map(|value| {
                value
                    .get::<String>()
                    .cloned()
                    .ok_or_else(|| "fanout supervisor argument is not a string".to_string())
            })
            .collect::<Result<Vec<_>, _>>()?;
        let timeout_secs = object
            .get("timeout_secs")
            .and_then(|value| value.get::<f64>())
            .copied()
            .map(|value| value as u64)
            .filter(|value| *value > 0)
            .ok_or_else(|| "fanout supervisor timeout is invalid".to_string())?;
        Ok(Self {
            reservation_id: required("reservation_id")?,
            cwd: required("cwd")?,
            tool: required("tool")?,
            command: required("command")?,
            arguments,
            preminted_session_id: config_string(&object, "preminted_session_id"),
            name: config_string(&object, "name"),
            timeout_secs,
        })
    }
}

fn create_managed_birth(
    store: &LifecycleStore,
    expected_runtime_session_id: Option<&str>,
    tool: &str,
    cwd: &str,
    expires_at_ms: i64,
    execution: ExecutionBackend,
) -> Result<ManagedBirth, String> {
    let birth = ManagedBirth {
        worker_id: store::uuid_v4(),
        generation: store::uuid_v4(),
        raw_token: format!("{}{}", store::uuid_v4(), store::uuid_v4()),
    };
    store.create_pending(
        PendingAttachSpec {
            worker_id: birth.worker_id.clone(),
            generation: birth.generation.clone(),
            expected_runtime_session_id: expected_runtime_session_id.map(str::to_string),
            expected_tool: tool.to_string(),
            expected_cwd: cwd.to_string(),
            expires_at_ms,
            required_scope: RequiredScope::ProcessOnly,
            execution,
        },
        &birth.raw_token,
    )?;
    Ok(birth)
}

fn claim_managed_appserver_birth(
    store: &LifecycleStore,
    birth: &ManagedBirth,
    runtime_session_id: &str,
    cwd: &str,
    name: Option<&str>,
    server: &str,
) -> Result<(), String> {
    match store.claim_managed_appserver_attach(
        ClaimManagedAttach {
            raw_token: birth.raw_token.clone(),
            worker_id: birth.worker_id.clone(),
            generation: birth.generation.clone(),
            runtime_session_id: runtime_session_id.to_string(),
            tool: "codex".to_string(),
            cwd: cwd.to_string(),
        },
        name,
        server,
    )? {
        ClaimOutcome::Active { worker, duplicate }
            if !duplicate
                && worker.worker_id == birth.worker_id
                && worker.generation == birth.generation
                && worker.runtime_session_id.as_deref() == Some(runtime_session_id)
                && worker.state == ManagedState::Active =>
        {
            Ok(())
        }
        ClaimOutcome::Active { .. } => {
            Err("managed app-server birth claim returned different authority".to_string())
        }
        ClaimOutcome::Refused { reason, .. } => {
            Err(format!("managed app-server birth claim refused: {reason}"))
        }
    }
}

fn managed_birth_expiry(timeout_secs: u64) -> i64 {
    let timeout_ms = i64::try_from(timeout_secs.saturating_mul(1000)).unwrap_or(i64::MAX);
    store::now_ms().saturating_add(timeout_ms)
}

pub(crate) const CHILD_ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "HOME",
    "CODEX_HOME",
    "CLAUDE_CONFIG_DIR",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "TMPDIR",
    "TERM",
    "LANG",
    "LC_ALL",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "ANTHROPIC_API_KEY",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "OPENAI_API_KEY",
    "AGENT_RELAY_HOME",
    "SESSION_RELAY_HOME",
    "RELAY_PLUGIN_ROOT",
    "RELAY_MANAGED_WORKER_ID",
    "RELAY_MANAGED_GENERATION",
    "RELAY_MANAGED_ATTACH_TOKEN",
    "RELAY_TEST_SENTINEL",
    "ATTACH_STUB_OUTPUT",
    "ATTACH_STUB_INTERACTIVE",
    "WAKE_STUB_DELAY_MS",
    "WAKE_STUB_FILE",
    "WAKE_STUB_RECORD",
    "WAKE_STUB_STATUS",
    "WAKE_STUB_STDERR",
    "WAKE_STUB_STDOUT",
    "STUB_RELAY_BIN",
];

/// Spawn, poll, cancel, and reap a transition-capable runtime child while the
/// sealed guard retains its binding/activity locks. Target authority and the
/// executable are derived exclusively from the guard.
pub fn run_child_with_guard(
    guard: &mut ReentryGuard,
    spec: ChildLaunchSpec,
) -> Result<Output, String> {
    crate::supervisor::run_child_with_guard(guard, spec)
}

fn die(msg: &str) -> ! {
    eprintln!("{msg}");
    std::process::exit(1);
}

const USAGE: &str = "usage: relay spawn <dir> [--fanout|--worktree --from <session>] [--tool claude|codex] [--model <m>] [--effort <e>] [--name <busName>] [--server <unix-socket>] [--reply-to <nameOrId>] [--timeout <sec>] [--read-only] [--full-access] [--watch] [--dry] [--] <first task>";

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

fn start_log_pump(log_id: &str) -> Result<(Child, ChildStdin, std::path::PathBuf), String> {
    let log_dir = store::home_dir().join("spawn-logs");
    fs::create_dir_all(&log_dir).map_err(|e| format!("create spawn-log dir: {e}"))?;
    let log_path = log_dir.join(format!("{log_id}.stderr"));
    fs::File::create(&log_path)
        .map_err(|e| format!("cannot create spawn log {}: {e}", log_path.display()))?;
    let relay_exe = std::env::current_exe().unwrap_or_else(|_| "relay".into());
    let mut writer = Command::new(relay_exe)
        .arg("__spawn-log-writer")
        .arg(log_id)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot launch spawn log writer: {e}"))?;
    let stdin = writer
        .stdin
        .take()
        .ok_or_else(|| "spawn log writer stdin unavailable".to_string())?;
    Ok((writer, stdin, log_path))
}

fn associate_spawn_log(log_path: &std::path::Path, id: &str) {
    let born_path = store::home_dir()
        .join("spawn-logs")
        .join(format!("{id}.stderr"));
    if log_path != born_path {
        if let Err(e) = fs::rename(log_path, &born_path) {
            eprintln!("[relay spawn] cannot associate spawn log with born session {id}: {e}");
        }
    }
}

/// Hidden stderr pump used by `relay spawn`. It is a separate process so the
/// pipe keeps draining after the parent relay returns at birth registration.
pub fn run_log_writer(id: &str) -> ! {
    if !store::is_uuid(id) {
        die("spawn log writer requires a UUID");
    }
    let dir = store::home_dir().join("spawn-logs");
    fs::create_dir_all(&dir).unwrap_or_else(|e| die(&format!("create spawn-log dir: {e}")));
    let path = dir.join(format!("{}.stderr", store::sanitize(id)));
    let mut file = store::with_lock(|| {
        let file = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| format!("open spawn log {}: {e}", path.display()))?;
        flock(&file, FlockOperation::LockShared)
            .map_err(|e| format!("flock spawn log {}: {e}", path.display()))?;
        Ok(file)
    })
    .unwrap_or_else(|e| die(&e));
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
    build_worker_prompt(
        reply_to,
        abs_relay,
        premint,
        WorkerWorkspace::CreateBranch,
        task,
    )
}

fn build_fanout_prompt(
    reply_to: &str,
    abs_relay: &str,
    premint: Option<&str>,
    task: &str,
) -> String {
    build_worker_prompt(
        reply_to,
        abs_relay,
        premint,
        WorkerWorkspace::AssignedFanoutWorktree {
            handback_session_id: premint.unwrap_or("<session-relay-id>"),
        },
        task,
    )
}

fn build_worker_prompt(
    reply_to: &str,
    abs_relay: &str,
    premint: Option<&str>,
    workspace: WorkerWorkspace<'_>,
    task: &str,
) -> String {
    let from = premint
        .map(|id| format!("--from {id} "))
        .unwrap_or_default();
    let (report_rule, workspace_rule, completion) = match workspace {
        WorkerWorkspace::CreateBranch => (
            format!(
                "When you finish, or if you need a decision, report to \"{reply_to}\" over the bus."
            ),
            "Always create and work on a separate git branch; never commit directly to the\n   default branch.".to_string(),
            String::new(),
        ),
        WorkerWorkspace::AssignedFanoutWorktree {
            handback_session_id,
        } => (
            format!(
                "If you need a decision, report to \"{reply_to}\" over the bus and wait for its reply."
            ),
            "Work only in the already assigned isolated worktree and branch; do not create or\n   switch branches.".to_string(),
            format!(
                r#"
Completion contract:
1. You must commit all intended changes on the assigned branch.
2. You must verify the worktree is clean.
3. Run:
   {abs_relay} handback --from {handback_session_id} --status completed --note "<brief note>"
   Replace <session-relay-id> with the identity supplied at SessionStart when needed.
This must be your final action; do not write to the worktree afterward.
"#
            ),
        ),
    };
    format!(
        r#"You are a session-relay bus worker spawned by "{reply_to}". You are running in a
fresh session in this project — its CLAUDE.md/AGENTS.md, skills, and plugins apply.
{report_rule}
PRIMARY (works even if session-relay isn't installed in this project) — run:
  {abs_relay} send "{reply_to}" {from}-- "<your message>"
(that is the absolute path to the relay binary that spawned you). If this project has
session-relay installed, the session-relay skill's send tool works too.

Guardrail rules (non-negotiable):
1. {workspace_rule}
2. Never modify live/production systems (e.g. over ssh). Read-only probes are
   allowed; mutations are not.
3. Destructive or irreversible operations require asking "{reply_to}" over the bus
   first and waiting for approval.
{completion}
Your task:

{task}"#
    )
}

fn appserver_spawn_config(options: &AppServerSpawn<'_>) -> String {
    let mut config: HashMap<String, JsonValue> = HashMap::new();
    for (key, value) in [
        ("server", Some(options.server)),
        ("dir", Some(options.dir)),
        ("name", options.name),
        ("reply_to", Some(options.reply_to)),
        ("task", Some(options.task)),
        ("model", options.model),
        ("effort", options.effort),
        ("sandbox", Some(options.sandbox)),
    ] {
        config.insert(
            key.to_string(),
            value
                .map(|value| JsonValue::from(value.to_string()))
                .unwrap_or(JsonValue::from(())),
        );
    }
    config.insert(
        "timeout_secs".into(),
        JsonValue::from(options.timeout_secs as f64),
    );
    JsonValue::from(config)
        .stringify()
        .unwrap_or_else(|_| "{}".to_string())
}

fn config_string(config: &HashMap<String, JsonValue>, key: &str) -> Option<String> {
    config
        .get(key)
        .and_then(|value| value.get::<String>())
        .cloned()
}

fn pump_report(key: &str, value: &str) {
    let mut report: HashMap<String, JsonValue> = HashMap::new();
    report.insert(key.to_string(), JsonValue::from(value.to_string()));
    println!(
        "{}",
        JsonValue::from(report)
            .stringify()
            .unwrap_or_else(|_| "{}".to_string())
    );
    let _ = std::io::stdout().flush();
}

fn pump_fail(message: &str) -> ! {
    pump_report("error", message);
    std::process::exit(1);
}

/// Hidden helper for app-server-native spawn. The foreground parent reads the
/// first stdout record and returns only after this process has confirmed the
/// initial turn/start. This process then keeps the SAME connection and shared
/// app-server event pump alive until completion or the bounded spawn timeout.
pub fn run_appserver_pump() -> ! {
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .unwrap_or_else(|e| pump_fail(&format!("read app-server spawn config: {e}")));
    let config = input
        .parse::<JsonValue>()
        .ok()
        .and_then(|value| value.get::<HashMap<String, JsonValue>>().cloned())
        .unwrap_or_else(|| pump_fail("invalid app-server spawn config"));
    let required = |key: &str| {
        config_string(&config, key)
            .unwrap_or_else(|| pump_fail(&format!("app-server spawn config missing {key}")))
    };
    let server = required("server");
    let dir = required("dir");
    let reply_to = required("reply_to");
    let task = required("task");
    let sandbox = required("sandbox");
    let name = config_string(&config, "name");
    let model = config_string(&config, "model");
    let effort = config_string(&config, "effort");
    let timeout_secs = config
        .get("timeout_secs")
        .and_then(|value| value.get::<f64>())
        .copied()
        .map(|value| value as u64)
        .unwrap_or(DEFAULT_TIMEOUT_SECS);

    let lifecycle = LifecycleStore::default();
    let birth = create_managed_birth(
        &lifecycle,
        None,
        "codex",
        &dir,
        managed_birth_expiry(timeout_secs),
        ExecutionBackend::SharedAppServer,
    )
    .unwrap_or_else(|error| pump_fail(&error));
    let mut spawned = appserver::start_thread(&server, &dir, model.as_deref(), &sandbox)
        .unwrap_or_else(|e| pump_fail(&e));
    let id = spawned.id().to_string();
    claim_managed_appserver_birth(&lifecycle, &birth, &id, &dir, name.as_deref(), &server)
        .unwrap_or_else(|error| pump_fail(&error));
    let mut guard = crate::lifecycle::admit_operation(&birth.worker_id, OperationKind::InitialTurn)
        .and_then(crate::lifecycle::Admission::into_guard)
        .unwrap_or_else(|error| pump_fail(&error));
    let abs_relay = std::env::current_exe()
        .ok()
        .map(|path| path.to_string_lossy().to_string())
        .unwrap_or_else(|| "relay".to_string());
    let prompt = build_prompt(&reply_to, &abs_relay, Some(&id), &task);
    if let Err(error) = spawned.start_initial_turn_with_guard(
        &mut guard,
        &prompt,
        model.as_deref(),
        effort.as_deref(),
    ) {
        let intent = guard
            .into_fence_intent("initial app-server turn/start failed")
            .unwrap_or_else(|fence_error| {
                pump_fail(&format!(
                    "publish failed turn/start fence after {error}: {fence_error}"
                ))
            });
        let permit =
            crate::lifecycle::drain_prior_operations(intent).unwrap_or_else(|drain_error| {
                pump_fail(&format!(
                    "drain failed turn/start fence after {error}: {drain_error:?}"
                ))
            });
        permit
            .mark_turn_unconfirmed(
                "turn/start failed before an exact turn identity could be recorded; no interrupt was attempted",
            )
            .unwrap_or_else(|mark_error| {
                pump_fail(&format!(
                    "retain unconfirmed turn/start fence after {error}: {mark_error}"
                ))
            });
        pump_fail(&error);
    }

    pump_report("started", &id);
    match spawned.pump_with_guard(&mut guard, timeout_secs.saturating_mul(1000)) {
        Ok(true) => std::process::exit(0),
        Ok(false) => {
            let turn_id = spawned
                .turn_id()
                .unwrap_or_else(|| pump_fail("initial turn identity was not recorded"))
                .to_string();
            let intent = guard
                .into_fence_intent("initial app-server turn timed out")
                .unwrap_or_else(|error| {
                    pump_fail(&format!("publish app-server timeout fence: {error}"))
                });
            let permit = crate::lifecycle::drain_prior_operations(intent).unwrap_or_else(|error| {
                pump_fail(&format!("drain app-server timeout fence: {error:?}"))
            });
            match spawned.interrupt_initial_turn(MANAGED_CANCEL_GRACE_MS) {
                Ok(true) => {
                    permit
                        .confirm_turn_terminal(&id, &turn_id, &id, &turn_id)
                        .unwrap_or_else(|error| {
                            pump_fail(&format!("confirm interrupted app-server turn: {error}"))
                        });
                }
                Ok(false) => {
                    permit
                        .mark_turn_unconfirmed(
                            "turn/interrupt returned but no matching turn/completed or idle state was observed",
                        )
                        .unwrap_or_else(|error| {
                            pump_fail(&format!("retain unconfirmed app-server fence: {error}"))
                        });
                }
                Err(error) => {
                    permit
                        .mark_turn_unconfirmed(&format!(
                            "turn/interrupt could not be confirmed: {error}"
                        ))
                        .unwrap_or_else(|mark_error| {
                            pump_fail(&format!(
                                "retain unconfirmed app-server fence: {mark_error}"
                            ))
                        });
                }
            }
            eprintln!("app-server first turn timed out after {timeout_secs}s");
            std::process::exit(124);
        }
        Err(error) => {
            let intent = guard
                .into_fence_intent("initial app-server turn pump failed")
                .unwrap_or_else(|fence_error| {
                    pump_fail(&format!(
                        "publish app-server pump failure fence after {error}: {fence_error}"
                    ))
                });
            let permit =
                crate::lifecycle::drain_prior_operations(intent).unwrap_or_else(|drain_error| {
                    pump_fail(&format!(
                        "drain app-server pump failure fence after {error}: {drain_error:?}"
                    ))
                });
            permit
                .mark_turn_unconfirmed(&format!(
                    "app-server connection failed after turn/start; terminal state is unknown: {error}"
                ))
                .unwrap_or_else(|mark_error| {
                    pump_fail(&format!(
                        "retain unconfirmed app-server pump failure after {error}: {mark_error}"
                    ))
                });
            pump_fail(&format!("app-server first turn pump failed: {error}"));
        }
    }
}

struct AppServerSpawn<'a> {
    server: &'a str,
    dir: &'a str,
    name: Option<&'a str>,
    reply_to: &'a str,
    task: &'a str,
    model: Option<&'a str>,
    effort: Option<&'a str>,
    sandbox: &'a str,
    timeout_secs: u64,
    watch: bool,
}

fn run_appserver_spawn(options: AppServerSpawn<'_>) -> ! {
    let provisional_id = store::uuid_v4();
    let (mut log_writer, log_stdin, log_path) =
        start_log_pump(&provisional_id).unwrap_or_else(|e| die(&e));
    let relay_exe = std::env::current_exe().unwrap_or_else(|_| "relay".into());
    let started_at = Instant::now();
    let mut pump = Command::new(relay_exe)
        .arg("__appserver-spawn-pump")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(log_stdin))
        .process_group(0)
        .spawn()
        .unwrap_or_else(|e| die(&format!("cannot launch app-server spawn pump: {e}")));
    let config = appserver_spawn_config(&options);
    let mut pump_stdin = pump
        .stdin
        .take()
        .unwrap_or_else(|| die("app-server spawn pump stdin unavailable"));
    pump_stdin
        .write_all(config.as_bytes())
        .unwrap_or_else(|e| die(&format!("write app-server spawn config: {e}")));
    drop(pump_stdin);

    let pump_stdout = pump
        .stdout
        .take()
        .unwrap_or_else(|| die("app-server spawn pump stdout unavailable"));
    let mut first_line = String::new();
    BufReader::new(pump_stdout)
        .read_line(&mut first_line)
        .unwrap_or_else(|e| die(&format!("read app-server spawn result: {e}")));
    let report = first_line
        .parse::<JsonValue>()
        .ok()
        .and_then(|value| value.get::<HashMap<String, JsonValue>>().cloned())
        .unwrap_or_default();
    if let Some(error) = config_string(&report, "error") {
        let status = pump.wait().ok();
        let _ = log_writer.wait();
        eprintln!(
            "app-server spawn failed: {error} — see {}",
            log_path.display()
        );
        std::process::exit(status.as_ref().map(exit_code).unwrap_or(1).max(1));
    }
    let Some(id) = config_string(&report, "started") else {
        let status = pump.wait().ok();
        let _ = log_writer.wait();
        eprintln!(
            "app-server spawn pump exited before confirming turn/start — see {}",
            log_path.display()
        );
        std::process::exit(status.as_ref().map(exit_code).unwrap_or(1).max(1));
    };
    associate_spawn_log(&log_path, &id);
    let display = options.name.unwrap_or(&id);
    if !options.watch {
        if let Some(name) = options.name {
            println!("spawned {name} ({id}) in {}", options.dir);
        } else {
            println!("spawned {id} in {}", options.dir);
        }
        std::process::exit(0);
    }

    let status = pump
        .wait()
        .unwrap_or_else(|e| die(&format!("cannot wait for app-server spawn pump: {e}")));
    let _ = log_writer.wait();
    let duration = elapsed(started_at);
    if status.success() {
        println!("spawned {display}; first turn complete; {duration}");
        std::process::exit(0);
    }
    let code = exit_code(&status);
    println!("spawned {display}; first turn failed (exit {code}); {duration}");
    std::process::exit(code);
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

fn run_fanout_spawn(options: FanoutLaunchOptions<'_>) -> ! {
    let fanout_store = FanoutStore::new(store::home_dir());
    let reservation = fanout::prepare_worktree(
        &fanout_store,
        options.repository,
        options.parent_session_id,
        options.mode,
    )
    .unwrap_or_else(|error| die(&error));
    let preminted_session_id = (options.tool != "codex").then(store::uuid_v4);
    let relay = std::env::current_exe()
        .ok()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| "relay".to_string());
    let prompt = build_fanout_prompt(
        options.reply_to,
        &relay,
        preminted_session_id.as_deref(),
        options.task,
    );
    let config = FanoutSpawnConfig {
        reservation_id: reservation.reservation_id,
        cwd: reservation.worktree.clone(),
        tool: options.tool.to_string(),
        command: child_cmd(options.tool),
        arguments: child_args(
            options.tool,
            options.permissions,
            preminted_session_id.as_deref(),
            false,
            options.model,
            options.effort,
            &prompt,
        ),
        preminted_session_id,
        name: options.name.map(str::to_string),
        timeout_secs: options.timeout_secs,
    };
    let executable = std::env::current_exe()
        .unwrap_or_else(|error| die(&format!("resolve relay executable: {error}")));
    let mut supervisor = Command::new(executable)
        .arg("__fanout-supervisor")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .unwrap_or_else(|error| die(&format!("launch fanout supervisor: {error}")));
    let mut input = supervisor
        .stdin
        .take()
        .unwrap_or_else(|| die("fanout supervisor stdin unavailable"));
    input
        .write_all(config.to_json().as_bytes())
        .unwrap_or_else(|error| die(&format!("write fanout supervisor config: {error}")));
    drop(input);
    let output = supervisor
        .stdout
        .take()
        .unwrap_or_else(|| die("fanout supervisor stdout unavailable"));
    let mut line = String::new();
    BufReader::new(output)
        .read_line(&mut line)
        .unwrap_or_else(|error| die(&format!("read fanout supervisor result: {error}")));
    let report = line
        .parse::<JsonValue>()
        .ok()
        .and_then(|value| value.get::<HashMap<String, JsonValue>>().cloned())
        .unwrap_or_default();
    if let Some(error) = config_string(&report, "error") {
        let _ = supervisor.wait();
        die(&error);
    }
    let id = config_string(&report, "started").unwrap_or_else(|| {
        let status = supervisor.wait().ok();
        die(&format!(
            "fanout supervisor exited before confirming an Active birth{}",
            status
                .as_ref()
                .map(|status| format!(" ({})", exit_code(status)))
                .unwrap_or_default()
        ));
    });
    drop(supervisor);
    if let Some(name) = options.name {
        println!("spawned {name} ({id}) in {}", reservation.worktree);
    } else {
        println!("spawned {id} in {}", reservation.worktree);
    }
    std::process::exit(0);
}

fn terminate_owned_child(child: &mut Child) -> Result<ExitStatus, String> {
    if child
        .try_wait()
        .map_err(|error| format!("inspect fanout child: {error}"))?
        .is_none()
    {
        child
            .kill()
            .map_err(|error| format!("terminate exact fanout child: {error}"))?;
    }
    child
        .wait()
        .map_err(|error| format!("reap exact fanout child: {error}"))
}

fn release_reaped_fanout_worker(
    lifecycle: &LifecycleStore,
    birth: &ManagedBirth,
    custody: &crate::lifecycle::OwnedProcessCustody,
    status: &ExitStatus,
    release: bool,
) -> Result<(), String> {
    lifecycle.record_owned_process_reaped(custody, exit_code(status))?;
    let intent = lifecycle.publish_fence(
        &birth.worker_id,
        &birth.generation,
        if release {
            "fanout handback"
        } else {
            "fanout process exited without handback"
        },
    )?;
    let permit = lifecycle
        .drain_prior_operations(intent)
        .map_err(|error| format!("drain fanout worker: {error:?}"))?;
    let fenced = permit.confirm_process_terminal()?;
    lifecycle.terminalize_worker(
        &birth.worker_id,
        &birth.generation,
        &fenced.version,
        if release {
            TerminalAction::Release
        } else {
            TerminalAction::Abandon
        },
        if release {
            "fanout process reaped"
        } else {
            "fanout process exited without handback"
        },
    )?;
    if release {
        Ok(())
    } else {
        Err("fanout process exited without handback; capacity retained".to_string())
    }
}

fn retain_reaped_fanout_worker(
    lifecycle: &LifecycleStore,
    birth: &ManagedBirth,
    reason: &str,
) -> Result<(), String> {
    let mut worker = lifecycle
        .read_worker(&birth.worker_id)?
        .ok_or_else(|| "fanout worker disappeared after reap".to_string())?;
    if worker.state == ManagedState::Active {
        let intent = lifecycle.publish_fence(&birth.worker_id, &birth.generation, reason)?;
        worker = lifecycle
            .drain_prior_operations(intent)
            .map_err(|error| format!("drain reaped fanout worker: {error:?}"))?
            .confirm_process_terminal()?;
    }
    if matches!(
        worker.state,
        ManagedState::Fenced | ManagedState::FencingUnconfirmed
    ) {
        lifecycle.terminalize_worker(
            &birth.worker_id,
            &birth.generation,
            &worker.version,
            TerminalAction::Abandon,
            reason,
        )?;
    }
    Ok(())
}

fn terminate_and_retain_fanout_child(
    lifecycle: &LifecycleStore,
    birth: &ManagedBirth,
    custody: &crate::lifecycle::OwnedProcessCustody,
    child: &mut Child,
    reason: &str,
) {
    if let Ok(status) = terminate_owned_child(child) {
        if lifecycle
            .record_owned_process_reaped(custody, exit_code(&status))
            .is_ok()
        {
            let _ = retain_reaped_fanout_worker(lifecycle, birth, reason);
        }
    }
}

fn supervise_fanout_child(
    fanout_store: &FanoutStore,
    lifecycle: &LifecycleStore,
    config: &FanoutSpawnConfig,
    birth: &ManagedBirth,
    custody: &crate::lifecycle::OwnedProcessCustody,
    child: &mut Child,
) -> Result<(), String> {
    loop {
        let record = fanout_store
            .read(&config.reservation_id)?
            .ok_or_else(|| "fanout reservation disappeared".to_string())?;
        if record.state == FanoutState::HandedBack {
            let permit = lifecycle
                .publish_fence(&birth.worker_id, &birth.generation, "fanout handback")
                .and_then(|intent| {
                    lifecycle
                        .drain_prior_operations(intent)
                        .map_err(|error| format!("drain fanout handback: {error:?}"))
                });
            let status = terminate_owned_child(child)?;
            lifecycle.record_owned_process_reaped(custody, exit_code(&status))?;
            let permit = match permit {
                Ok(permit) => permit,
                Err(error) => {
                    let _ = retain_reaped_fanout_worker(
                        lifecycle,
                        birth,
                        "fanout handback drain was not proven",
                    );
                    return Err(format!(
                        "{error}; exact process reaped but capacity retained"
                    ));
                }
            };
            let fenced = permit.confirm_process_terminal()?;
            lifecycle.terminalize_worker(
                &birth.worker_id,
                &birth.generation,
                &fenced.version,
                TerminalAction::Release,
                "fanout process reaped",
            )?;
            return Ok(());
        }
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("inspect fanout child: {error}"))?
        {
            let release = fanout_store
                .read(&config.reservation_id)?
                .is_some_and(|current| current.state == FanoutState::HandedBack);
            return release_reaped_fanout_worker(lifecycle, birth, custody, &status, release);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

pub fn run_fanout_supervisor() -> ! {
    let config = FanoutSpawnConfig::from_stdin().unwrap_or_else(|error| pump_fail(&error));
    let fanout_store = FanoutStore::new(store::home_dir());
    let lifecycle = LifecycleStore::default();
    let birth = create_managed_birth(
        &lifecycle,
        config.preminted_session_id.as_deref(),
        &config.tool,
        &config.cwd,
        managed_birth_expiry(config.timeout_secs),
        ExecutionBackend::SupervisorOwnedProcess,
    )
    .unwrap_or_else(|error| pump_fail(&format!("create fanout managed birth: {error}")));
    fanout_store
        .bind_managed(&config.reservation_id, &birth.worker_id, &birth.generation)
        .unwrap_or_else(|error| pump_fail(&format!("bind fanout managed birth: {error}")));
    let log_id = config
        .preminted_session_id
        .clone()
        .unwrap_or_else(|| config.reservation_id.clone());
    let (mut log_writer, log_stdin, log_path) = match start_log_pump(&log_id) {
        Ok(log) => log,
        Err(error) => {
            let rollback = fanout::rollback_before_process_start(
                &fanout_store,
                &config.reservation_id,
                &birth.worker_id,
                &birth.generation,
                &error,
            );
            pump_fail(&match rollback {
                Ok(_) => error,
                Err(rollback_error) => {
                    format!("{error}; rollback retained capacity: {rollback_error}")
                }
            });
        }
    };
    let marker_before = store::id_for_dir(&config.cwd);
    let mut command = Command::new(&config.command);
    command
        .args(&config.arguments)
        .current_dir(&config.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log_stdin))
        .process_group(0);
    for key in [
        "RELAY_MANAGED_WORKER_ID",
        "RELAY_MANAGED_GENERATION",
        "RELAY_MANAGED_ATTACH_TOKEN",
    ] {
        command.env_remove(key);
    }
    command
        .env("RELAY_MANAGED_WORKER_ID", &birth.worker_id)
        .env("RELAY_MANAGED_GENERATION", &birth.generation)
        .env("RELAY_MANAGED_ATTACH_TOKEN", &birth.raw_token);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            drop(command);
            let _ = log_writer.wait();
            let message = format!("failed to launch {}: {error}", config.command);
            let rollback = fanout::rollback_before_process_start(
                &fanout_store,
                &config.reservation_id,
                &birth.worker_id,
                &birth.generation,
                &message,
            );
            pump_fail(&match rollback {
                Ok(_) => message,
                Err(rollback_error) => {
                    format!("{message}; rollback retained capacity: {rollback_error}")
                }
            });
        }
    };
    drop(command);
    let supervisor_instance_id = store::uuid_v4();
    let custody = match lifecycle.begin_owned_process_custody(
        &birth.worker_id,
        &birth.generation,
        &supervisor_instance_id,
        crate::supervisor::observe_process(child.id()),
    ) {
        Ok(custody) => custody,
        Err(error) => {
            let _ = terminate_owned_child(&mut child);
            let _ = log_writer.wait();
            let message = format!("publish fanout process custody: {error}");
            let _ = fanout_store.record_error(&config.reservation_id, &message);
            pump_fail(&message);
        }
    };
    let deadline = Instant::now() + Duration::from_secs(config.timeout_secs);
    let id = loop {
        let born = match config.preminted_session_id.as_ref() {
            Some(id) => store::resolve(id).map(|entry| entry.id),
            None => match store::id_for_dir(&config.cwd) {
                Some(id) if Some(&id) != marker_before.as_ref() && store::is_uuid(&id) => Some(id),
                _ => None,
            },
        };
        if let Some(id) = born {
            break id;
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                let _ = lifecycle.record_owned_process_reaped(&custody, exit_code(&status));
                let _ = log_writer.wait();
                let message = format!(
                    "fanout child exited before birth registration ({}) — see {}",
                    exit_code(&status),
                    log_path.display()
                );
                let _ = fanout_store.record_error(&config.reservation_id, &message);
                pump_fail(&message);
            }
            Ok(None) => {}
            Err(error) => pump_fail(&format!("inspect fanout child birth: {error}")),
        }
        if Instant::now() >= deadline {
            let status = terminate_owned_child(&mut child);
            if let Ok(status) = &status {
                let _ = lifecycle.record_owned_process_reaped(&custody, exit_code(status));
            }
            let _ = log_writer.wait();
            let message = format!(
                "no fanout birth registration within {}s — see {}",
                config.timeout_secs,
                log_path.display()
            );
            let _ = fanout_store.record_error(&config.reservation_id, &message);
            pump_fail(&message);
        }
        std::thread::sleep(Duration::from_millis(BIRTH_POLL_MS));
    };
    let active = lifecycle
        .read_worker(&birth.worker_id)
        .ok()
        .flatten()
        .is_some_and(|worker| {
            worker.generation == birth.generation
                && worker.runtime_session_id.as_deref() == Some(id.as_str())
                && worker.state == ManagedState::Active
        });
    if !active {
        terminate_and_retain_fanout_child(
            &lifecycle,
            &birth,
            &custody,
            &mut child,
            "fanout managed claim was not exact",
        );
        let _ = log_writer.wait();
        let message = "fanout SessionStart did not complete its exact managed claim";
        let _ = fanout_store.record_error(&config.reservation_id, message);
        pump_fail(message);
    }
    if let Err(error) = fanout_store.attach_runtime(&config.reservation_id, &id) {
        terminate_and_retain_fanout_child(
            &lifecycle,
            &birth,
            &custody,
            &mut child,
            "fanout runtime attachment failed",
        );
        let _ = log_writer.wait();
        let message = format!("attach fanout runtime: {error}");
        let _ = fanout_store.record_error(&config.reservation_id, &message);
        pump_fail(&message);
    }
    associate_spawn_log(&log_path, &id);
    if config.name.is_some() {
        if let Err(error) = store::register(
            &id,
            Some(&config.cwd),
            config.name.as_deref(),
            Some(&config.tool),
            None,
        ) {
            terminate_and_retain_fanout_child(
                &lifecycle,
                &birth,
                &custody,
                &mut child,
                "fanout name registration failed",
            );
            let _ = log_writer.wait();
            let _ = fanout_store.record_error(&config.reservation_id, &error);
            pump_fail(&error);
        }
    }
    pump_report("started", &id);
    let result = supervise_fanout_child(
        &fanout_store,
        &lifecycle,
        &config,
        &birth,
        &custody,
        &mut child,
    );
    let _ = log_writer.wait();
    if let Err(error) = result {
        let _ = fanout_store.record_error(&config.reservation_id, &error);
        std::process::exit(1);
    }
    std::process::exit(0);
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
    let server = args.flag("server");
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
        .or_else(|| args.flag("from").map(str::to_string))
        .or_else(default_reply_to)
        .unwrap_or_else(|| {
            die("no --reply-to given and this directory has no registered session; pass --reply-to <name-or-id> (see `relay whoami`)")
        });

    let abs_relay = std::env::current_exe()
        .ok()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| "relay".to_string());

    let perm = perm_args(&tool, read_only, full_access);
    let fanout_mode = match (args.has("fanout"), args.has("worktree")) {
        (true, false) => Some(FanoutMode::Root),
        (false, true) => Some(FanoutMode::Child),
        (false, false) => None,
        (true, true) => die("--fanout and --worktree are mutually exclusive"),
    };
    if let Some(mode) = fanout_mode {
        let parent_session_id = args
            .flag("from")
            .unwrap_or_else(|| die("fanout spawn requires --from <session UUID>"));
        if server.is_some() {
            die("fanout spawn does not support --server");
        }
        if read_only {
            die("fanout spawn does not support --read-only");
        }
        if args.has("dry") {
            die("fanout spawn does not support --dry");
        }
        if watch {
            die("fanout spawn returns after exact Active birth; --watch is not supported");
        }
        run_fanout_spawn(FanoutLaunchOptions {
            repository: &dir,
            mode,
            parent_session_id,
            tool: &tool,
            model,
            effort,
            name: args.flag("name"),
            reply_to: &reply_to,
            timeout_secs,
            permissions: &perm,
            task: &task,
        });
    }
    if tool == "codex" {
        if let Some(server) = server {
            if args.has("dry") {
                let mut out: HashMap<String, JsonValue> = HashMap::new();
                for (key, value) in [
                    ("action", "app-server-spawn"),
                    ("tool", "codex"),
                    ("server", server),
                    ("cwd", &dir_s),
                    ("sandbox", &perm[1]),
                    ("task", &task),
                ] {
                    out.insert(key.into(), JsonValue::from(value.to_string()));
                }
                if let Some(model) = model {
                    out.insert("model".into(), JsonValue::from(model.to_string()));
                }
                if let Some(effort) = effort {
                    out.insert("effort".into(), JsonValue::from(effort.to_string()));
                }
                println!(
                    "{}",
                    JsonValue::from(out)
                        .stringify()
                        .unwrap_or_else(|_| "{}".to_string())
                );
                std::process::exit(0);
            }
            run_appserver_spawn(AppServerSpawn {
                server,
                dir: &dir_s,
                name: args.flag("name"),
                reply_to: &reply_to,
                task: &task,
                model,
                effort,
                sandbox: &perm[1],
                timeout_secs,
                watch,
            });
        }
    }

    // Claude accepts a pre-minted id (watch for exactly it); codex has no
    // pre-set-id flag, so birth is detected by the dir marker changing.
    let premint = (tool != "codex").then(store::uuid_v4);
    let prompt = build_prompt(&reply_to, &abs_relay, premint.as_deref(), &task);
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
        if let Some(server) = server {
            m.insert(
                "server".into(),
                tinyjson::JsonValue::from(server.to_string()),
            );
        }
        println!(
            "{}",
            tinyjson::JsonValue::from(m).stringify().unwrap_or_default()
        );
        std::process::exit(0);
    }

    let lifecycle = LifecycleStore::default();
    let managed_birth = Some(
        create_managed_birth(
            &lifecycle,
            premint.as_deref(),
            &tool,
            &dir_s,
            managed_birth_expiry(timeout_secs),
            ExecutionBackend::ObservationOnly,
        )
        .unwrap_or_else(|error| die(&format!("create managed {tool} birth: {error}"))),
    );

    // stderr → a spawn-log so a child that execs then dies fast (bad flag,
    // auth failure) stays diagnosable; stdout/stdin are null by design.
    let log_id = premint.clone().unwrap_or_else(store::uuid_v4);
    let (mut log_writer, log_stdin, log_path) = start_log_pump(&log_id).unwrap_or_else(|e| die(&e));

    let marker_before = store::id_for_dir(&dir_s);
    let mut command = Command::new(&cmd);
    command
        .args(&cargs)
        .current_dir(&dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log_stdin))
        .process_group(0);
    for key in [
        "RELAY_MANAGED_WORKER_ID",
        "RELAY_MANAGED_GENERATION",
        "RELAY_MANAGED_ATTACH_TOKEN",
    ] {
        command.env_remove(key);
    }
    if let Some(birth) = managed_birth.as_ref() {
        command
            .env("RELAY_MANAGED_WORKER_ID", &birth.worker_id)
            .env("RELAY_MANAGED_GENERATION", &birth.generation)
            .env("RELAY_MANAGED_ATTACH_TOKEN", &birth.raw_token);
    }
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
    if let Some(birth) = managed_birth.as_ref() {
        let active = lifecycle
            .read_worker(&birth.worker_id)
            .ok()
            .flatten()
            .is_some_and(|worker| {
                worker.generation == birth.generation
                    && worker.runtime_session_id.as_deref() == Some(id.as_str())
                    && worker.state == ManagedState::Active
            });
        if !active {
            let _ = child.kill();
            let _ = child.wait();
            die(&format!(
                "{tool} SessionStart registered without completing its exact managed claim"
            ));
        }
    }
    associate_spawn_log(&log_path, &id);
    let name = args.flag("name");
    if name.is_some() || server.is_some() {
        store::register(&id, Some(&dir_s), name, Some(&tool), server).unwrap_or_else(|e| die(&e));
    }
    let display = name.map(str::to_string).unwrap_or_else(|| id.clone());
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
    fn fanout_prompt_requires_a_clean_committed_handback_as_the_final_action() {
        let p = build_fanout_prompt("boss", "/opt/bin/relay", Some("u-7"), "do the thing");
        assert!(p.contains("already assigned isolated worktree and branch"));
        assert!(!p.contains("Always create and work on a separate git branch"));
        assert!(p.contains("commit all intended changes"));
        assert!(p.contains("verify the worktree is clean"));
        assert!(p.contains(
            r#"/opt/bin/relay handback --from u-7 --status completed --note "<brief note>""#
        ));
        assert!(p.contains("must be your final action; do not write to the worktree afterward"));
        assert!(p.trim_end().ends_with("do the thing"));
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
