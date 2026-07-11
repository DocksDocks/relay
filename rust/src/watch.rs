// watch.rs — `relay watch`: zero-keystroke push of relay mail into LIVE Codex
// threads hosted under `codex app-server` (the maintainer-endorsed automation
// seam — the plain TUI cannot be injected into, openai/codex#11415).
// Spike-verified 2026-07-02 on codex-cli 0.142.5:
//   - every app-server socket listener (unix:// included) speaks WebSocket —
//     HTTP Upgrade + RFC6455 frames; raw JSONL exists only on stdio. Hence the
//     hand-rolled WS CLIENT below (zero-crate budget: /dev/urandom supplies the
//     key + masks; the Sec-WebSocket-Accept SHA1 check is intentionally
//     skipped — local socket, the 101 status line is the gate).
//   - `thread/resume` accepts a raw rollout/session uuid (thread id == the
//     relay registry id); `thread/inject_items` persists durably and is
//     model-visible; `turn/start` with approvalPolicy "never" completes
//     unattended. The `jsonrpc` field is omitted on the wire.
//   - a `turn/start` issued immediately after inject_items wedges the turn:
//     wait RELAY_TURN_SETTLE_MS (default 5000) between the two.
//   - `approvalPolicy: "never"` auto-rejects shell approvals, but an MCP tool
//     call raises an `mcpServer/elicitation/request` server->client REQUEST
//     that MUST be answered (`{action: "accept"|"decline"}`) or the turn wedges
//     on `waitingOnApproval` forever (live-reproduced 2026-07-02). So after
//     `turn/start`, watch stays attached until `turn/completed`, accepting
//     elicitations for the relay's own `bus` server (store-local tools only)
//     and declining every other server — a declined call fails cleanly and the
//     turn continues.
// Delivery: default = inject_items with the UNTRUSTED-DATA fence (mail waits
// for the thread's next turn); --auto-turn additionally starts a turn carrying
// a neutral acknowledgement (never mail content). Status is checked before
// inject and again before turn/start. This shrinks, but cannot close, the
// cross-client race with a simultaneous human turn/start. Targets that are not
// app-server reachable fall back to the wake doorbell. A successful inject is
// final: only a failure before inject succeeds may re-enqueue mail.

use crate::appserver;
use crate::cli::{Args, DEFAULT_NUDGE};
use crate::hook;
use crate::lifecycle::{self, ChildLaunchSpec, DoorbellMessage, OperationKind};
use crate::sha256::Sha256;
use crate::spawn;
use crate::store;
use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tinyjson::JsonValue;

const POLL_MS: u64 = 2000;
const DEFAULT_SETTLE_MS: u64 = 5000;
const WAKE_RETRY_MAX_MS: u64 = 30_000;
const FOLLOW_READ_BUFFER_BYTES: usize = 64 * 1024;
const MAX_FOLLOW_PENDING_BYTES: usize = 8 * 1024 * 1024;

fn die(msg: &str) -> ! {
    eprintln!("{msg}");
    std::process::exit(1);
}

struct Target {
    id: String,
    tool: String,
    dir: Option<String>,
    server: Option<String>,
    allow_bus: bool,
}

#[derive(PartialEq, Debug, Clone, Copy)]
enum Mode {
    Push,
    Wake,
}

struct FollowFile {
    file: std::fs::File,
    dev: u64,
    ino: u64,
    pending: Vec<u8>,
    dropping_overlong: bool,
    prefix_hash: Sha256,
    snapshot: FileSnapshot,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct FileSnapshot {
    len: u64,
    mtime: i64,
    mtime_nsec: i64,
    ctime: i64,
    ctime_nsec: i64,
}

impl FileSnapshot {
    fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        Self {
            len: metadata.len(),
            mtime: metadata.mtime(),
            mtime_nsec: metadata.mtime_nsec(),
            ctime: metadata.ctime(),
            ctime_nsec: metadata.ctime_nsec(),
        }
    }
}

struct WakeRetry {
    refusals: u32,
    next_at: Instant,
}

enum WakeOutcome {
    Delivered,
    Refused,
    Failed,
}

enum PushOutcome {
    Delivered,
    Busy,
    AckDeferred(Option<String>),
}

// Reachability: only a codex session hosted under an app-server can take a
// push; everything else gets the doorbell. `--server` implies tool=codex on
// the --id path (an unregistered id would otherwise default to claude and
// silently route to the wrong leg).
fn decide(tool: &str, server: Option<&str>) -> Mode {
    if tool == "codex" && server.is_some() {
        Mode::Push
    } else {
        Mode::Wake
    }
}

fn validate_tool(tool: &str) {
    if !matches!(tool, "claude" | "codex") {
        die(&format!("--tool must be claude|codex, got: {tool}"));
    }
}

fn resolve_targets(args: &Args, server: Option<&str>) -> Vec<Target> {
    if let Some(id) = args.flag("id") {
        if !store::is_uuid(id) {
            die(&format!("--id must be a session UUID, got: {id}"));
        }
        let tool = args
            .flag("tool")
            .map(str::to_string)
            .unwrap_or_else(|| if server.is_some() { "codex" } else { "claude" }.to_string());
        return vec![Target {
            id: id.to_string(),
            tool,
            dir: args.flag("dir").map(str::to_string),
            server: server.map(str::to_string),
            allow_bus: false,
        }];
    }
    if args.has("all") {
        return store::roster()
            .into_iter()
            .filter(|e| server.is_none() || e.tool == "codex")
            .map(|e| {
                let allow_bus = e.spawned_via.as_deref() == Some("app-server");
                Target {
                    id: e.id,
                    tool: e.tool,
                    dir: e.dir,
                    server: e.server.or_else(|| server.map(str::to_string)),
                    allow_bus,
                }
            })
            .collect();
    }
    args.positionals(1)
        .iter()
        .map(|who| {
            let Some(e) = store::resolve(who) else {
                die(&format!("unknown session: {who}"));
            };
            let allow_bus = e.spawned_via.as_deref() == Some("app-server");
            Target {
                id: e.id,
                tool: args.flag("tool").map(str::to_string).unwrap_or(e.tool),
                dir: e.dir,
                server: e.server.or_else(|| server.map(str::to_string)),
                allow_bus,
            }
        })
        .collect()
}

fn materialize_appserver_authority(target: &Target) -> Result<(), String> {
    let Some(server) = target.server.as_deref() else {
        return Ok(());
    };
    if let Some(entry) = store::resolve(&target.id) {
        if entry.server.as_deref() == Some(server) {
            return Ok(());
        }
        store::register(
            &target.id,
            entry.dir.as_deref(),
            None,
            Some(&entry.tool),
            Some(server),
        )?;
        return Ok(());
    }
    if crate::lifecycle::LifecycleStore::default()
        .read_binding(&target.id)?
        .is_some()
    {
        return Err("app-server target has lifecycle state but no registry entry".to_string());
    }
    let dir = target
        .dir
        .as_deref()
        .ok_or_else(|| "unregistered app-server target requires --dir".to_string())?;
    store::register(
        &target.id,
        Some(dir),
        None,
        Some(&target.tool),
        Some(server),
    )?;
    Ok(())
}

pub fn run(raw: Vec<String>) -> ! {
    let args = Args(raw);
    if let Some(id) = args.flag("follow") {
        if !store::is_uuid(id) {
            die(&format!("--follow must be a session UUID, got: {id}"));
        }
        if args.has("all") || args.has("once") || args.flag("server").is_some() {
            die("--follow cannot be combined with --all, --once, or --server");
        }
        let tool = args.flag("tool").unwrap_or("claude");
        validate_tool(tool);
        let _guard = store::acquire_watcher_lock(id, tool, "follow")
            .unwrap_or_else(|e| die(&format!("cannot follow {id}: {e}")));
        follow_mailbox(id);
    }
    let fallback_server = args.flag("server").map(str::to_string).or_else(|| {
        std::env::var("RELAY_APP_SERVER")
            .ok()
            .filter(|v| !v.is_empty())
    });
    let auto_turn = args.has("auto-turn");
    let once = args.has("once");
    let dry = args.has("dry");
    let settle_ms: u64 = std::env::var("RELAY_TURN_SETTLE_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_SETTLE_MS);

    let targets = resolve_targets(&args, fallback_server.as_deref());
    if targets.is_empty() {
        die(
            "usage: relay watch <nameOrId>... | --all | --id <uuid> [--server <unix-socket>] [--tool codex] [--auto-turn] [--once] [--dry]",
        );
    }

    let mut guards = Vec::new();
    let mut active_targets = Vec::new();
    for target in targets {
        validate_tool(&target.tool);
        if dry {
            active_targets.push(target);
            continue;
        }
        materialize_appserver_authority(&target)
            .unwrap_or_else(|error| die(&format!("cannot bind app-server authority: {error}")));
        let mode = if once { "once" } else { "doorbell" };
        match store::acquire_watcher_lock(&target.id, &target.tool, mode) {
            Ok(guard) => {
                guards.push(guard);
                active_targets.push(target);
            }
            Err(store::LockAcquireError::Busy(_)) if args.has("all") => {
                eprintln!("[relay watch] skipping {}: watcher already live", target.id);
            }
            Err(e) => die(&format!("cannot watch {}: {e}", target.id)),
        }
    }
    if active_targets.is_empty() {
        std::process::exit(0);
    }

    // A woken target keeps its mail until its own hook drains it — don't
    // re-ring the doorbell every poll tick while that is in flight.
    let mut woken: HashSet<String> = HashSet::new();
    let mut wake_retries: HashMap<String, WakeRetry> = HashMap::new();
    let mut pending_ack: HashSet<String> = HashSet::new();
    let mut had_error = false;
    loop {
        for t in &active_targets {
            if let Err(e) = store::update_watcher_progress(&t.id) {
                eprintln!("[relay watch] progress update for {} failed: {e}", t.id);
            }
            if pending_ack.contains(&t.id) {
                let Some(server) = t.server.as_deref() else {
                    eprintln!(
                        "[relay watch] pending acknowledgement for {} has no app-server",
                        t.id
                    );
                    continue;
                };
                if appserver::probe(server).is_err() {
                    continue;
                }
                let mut guard = match lifecycle::admit_operation(&t.id, OperationKind::WatchAck)
                    .and_then(lifecycle::Admission::into_guard)
                {
                    Ok(guard) => guard,
                    Err(error) => {
                        eprintln!(
                            "[relay watch] acknowledgement for {} refused: {error}",
                            t.id
                        );
                        continue;
                    }
                };
                match appserver::acknowledge_with_guard(&mut guard, t.allow_bus) {
                    Ok(appserver::DeliveryOutcome::Delivered) => {
                        pending_ack.remove(&t.id);
                    }
                    Ok(appserver::DeliveryOutcome::AckDeferred) => continue,
                    Err(e) => {
                        eprintln!("[relay watch] acknowledgement for {} deferred: {e}", t.id);
                        continue;
                    }
                }
            }
            if !store::mailbox_has_content(&t.id) {
                woken.remove(&t.id);
                wake_retries.remove(&t.id);
                continue;
            }
            let configured_mode = decide(&t.tool, t.server.as_deref());
            let mode = if configured_mode == Mode::Push
                && !dry
                && appserver::probe(t.server.as_deref().unwrap()).is_err()
            {
                Mode::Wake
            } else {
                configured_mode
            };
            match mode {
                Mode::Push => {
                    match push_target(t.server.as_deref().unwrap(), t, auto_turn, dry, settle_ms) {
                        Ok(PushOutcome::Delivered) => {}
                        Ok(PushOutcome::AckDeferred(reason)) => {
                            if let Some(reason) = reason {
                                eprintln!(
                                    "[relay watch] mail delivered to {}; visible acknowledgement deferred: {reason}",
                                    t.id
                                );
                            }
                            if !once {
                                pending_ack.insert(t.id.clone());
                            }
                        }
                        Ok(PushOutcome::Busy) => {
                            if once {
                                had_error = true;
                            }
                        }
                        Err(e) => {
                            eprintln!("[relay watch] {e}");
                            had_error = true;
                        }
                    }
                }
                Mode::Wake => {
                    if woken.contains(&t.id) {
                        continue;
                    }
                    if wake_retries
                        .get(&t.id)
                        .is_some_and(|retry| Instant::now() < retry.next_at)
                    {
                        continue;
                    }
                    match wake_fallback(t, dry) {
                        WakeOutcome::Delivered => {
                            wake_retries.remove(&t.id);
                            woken.insert(t.id.clone());
                        }
                        WakeOutcome::Refused => {
                            if once {
                                had_error = true;
                                continue;
                            }
                            let retry = wake_retries.entry(t.id.clone()).or_insert(WakeRetry {
                                refusals: 0,
                                next_at: Instant::now(),
                            });
                            retry.refusals = retry.refusals.saturating_add(1);
                            let shift = retry.refusals.saturating_sub(1).min(4);
                            let delay_ms = POLL_MS
                                .saturating_mul(1_u64 << shift)
                                .min(WAKE_RETRY_MAX_MS);
                            retry.next_at = Instant::now() + Duration::from_millis(delay_ms);
                            eprintln!(
                                "[relay watch] wake refused for {} (fallback); retrying in {}ms",
                                t.id, delay_ms
                            );
                        }
                        WakeOutcome::Failed => {
                            wake_retries.remove(&t.id);
                            woken.insert(t.id.clone());
                            had_error = true;
                        }
                    }
                }
            }
        }
        if once {
            std::process::exit(if had_error { 1 } else { 0 });
        }
        std::thread::sleep(Duration::from_millis(POLL_MS));
    }
}

fn read_follow_bytes(state: &mut FollowFile, out: &mut impl Write) -> Result<(), String> {
    let mut added = [0_u8; FOLLOW_READ_BUFFER_BYTES];
    loop {
        let read = state
            .file
            .read(&mut added)
            .map_err(|e| format!("read followed mailbox: {e}"))?;
        if read == 0 {
            break;
        }
        state.prefix_hash.update(&added[..read]);
        for byte in &added[..read] {
            if state.dropping_overlong {
                if *byte == b'\n' {
                    state.dropping_overlong = false;
                }
                continue;
            }
            if *byte == b'\n' {
                out.write_all(&state.pending)
                    .and_then(|()| out.write_all(b"\n"))
                    .and_then(|()| out.flush())
                    .map_err(|e| format!("write followed mailbox line: {e}"))?;
                state.pending.clear();
            } else if state.pending.len() < MAX_FOLLOW_PENDING_BYTES {
                state.pending.push(*byte);
            } else {
                state.pending.clear();
                state.dropping_overlong = true;
                eprintln!(
                    "[relay watch] followed mailbox record exceeded {} bytes; dropping through newline",
                    MAX_FOLLOW_PENDING_BYTES
                );
            }
        }
    }
    let metadata = state
        .file
        .metadata()
        .map_err(|e| format!("stat followed mailbox after read: {e}"))?;
    state.snapshot = FileSnapshot::from_metadata(&metadata);
    Ok(())
}

fn digest_followed_prefix(
    state: &mut FollowFile,
    prefix_len: u64,
) -> Result<Option<[u8; 32]>, String> {
    let offset = state
        .file
        .stream_position()
        .map_err(|e| format!("read followed mailbox position: {e}"))?;
    state
        .file
        .seek(SeekFrom::Start(0))
        .map_err(|e| format!("seek followed mailbox prefix: {e}"))?;

    let mut hasher = Sha256::new();
    let mut remaining = prefix_len;
    let mut buffer = [0_u8; 8192];
    let result = loop {
        if remaining == 0 {
            break Ok(Some(hasher.digest()));
        }
        let want = remaining.min(buffer.len() as u64) as usize;
        match state.file.read(&mut buffer[..want]) {
            Ok(0) => break Ok(None),
            Ok(read) => {
                hasher.update(&buffer[..read]);
                remaining -= read as u64;
            }
            Err(e) => break Err(format!("read followed mailbox prefix: {e}")),
        }
    };
    state
        .file
        .seek(SeekFrom::Start(offset))
        .map_err(|e| format!("restore followed mailbox position: {e}"))?;
    result
}

fn followed_content_changed(
    state: &mut FollowFile,
    metadata: &std::fs::Metadata,
) -> Result<bool, String> {
    if state.snapshot == FileSnapshot::from_metadata(metadata) {
        return Ok(false);
    }
    let offset = state
        .file
        .stream_position()
        .map_err(|e| format!("read followed mailbox position: {e}"))?;
    if metadata.len() < offset {
        return Ok(true);
    }
    let actual = digest_followed_prefix(state, offset)?;
    Ok(actual.is_none_or(|digest| digest != state.prefix_hash.digest()))
}

fn open_follow_file(path: &Path, skip_existing: bool) -> Result<FollowFile, String> {
    let mut file = std::fs::File::open(path)
        .map_err(|e| format!("open followed mailbox {}: {e}", path.display()))?;
    let mut prefix_hash = Sha256::new();
    if skip_existing {
        let mut buffer = [0_u8; 8192];
        loop {
            let read = file
                .read(&mut buffer)
                .map_err(|e| format!("read existing mailbox {}: {e}", path.display()))?;
            if read == 0 {
                break;
            }
            prefix_hash.update(&buffer[..read]);
        }
    }
    let metadata = file
        .metadata()
        .map_err(|e| format!("restat followed mailbox {}: {e}", path.display()))?;
    Ok(FollowFile {
        file,
        dev: metadata.dev(),
        ino: metadata.ino(),
        pending: Vec::new(),
        dropping_overlong: false,
        prefix_hash,
        snapshot: FileSnapshot::from_metadata(&metadata),
    })
}

fn follow_mailbox(id: &str) -> ! {
    let path: PathBuf = store::mailbox_path(id);
    let mut skip_first_open = path.exists();
    let mut state: Option<FollowFile> = None;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    loop {
        if let Err(e) = store::update_watcher_progress(id) {
            eprintln!("[relay watch] progress update for {id} failed: {e}");
        }
        match std::fs::metadata(&path) {
            Ok(metadata) => {
                let replaced = state
                    .as_ref()
                    .map(|s| s.dev != metadata.dev() || s.ino != metadata.ino())
                    .unwrap_or(true);
                if replaced {
                    match open_follow_file(&path, skip_first_open) {
                        Ok(opened) => {
                            state = Some(opened);
                            skip_first_open = false;
                        }
                        Err(e) => eprintln!("[relay watch] {e}"),
                    }
                } else if let Some(current) = state.as_mut() {
                    let content_changed =
                        followed_content_changed(current, &metadata).unwrap_or_else(|e| die(&e));
                    if content_changed {
                        current
                            .file
                            .seek(SeekFrom::Start(0))
                            .unwrap_or_else(|e| die(&format!("reset followed mailbox: {e}")));
                        current.pending.clear();
                        current.dropping_overlong = false;
                        current.prefix_hash = Sha256::new();
                    }
                }
                if let Some(current) = state.as_mut() {
                    if let Err(e) = read_follow_bytes(current, &mut out) {
                        die(&e);
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if let Some(current) = state.as_mut() {
                    if let Err(e) = read_follow_bytes(current, &mut out) {
                        die(&e);
                    }
                }
                state = None;
            }
            Err(e) => eprintln!("[relay watch] stat {}: {e}", path.display()),
        }
        std::thread::sleep(Duration::from_millis(POLL_MS));
    }
}

fn push_target(
    server: &str,
    t: &Target,
    auto_turn: bool,
    dry: bool,
    settle_ms: u64,
) -> Result<PushOutcome, String> {
    if dry {
        println!(
            "{}",
            str_obj(&[
                ("action", if auto_turn { "auto-turn" } else { "inject" }),
                ("id", &t.id),
                ("server", server),
            ])
        );
        return Ok(PushOutcome::Delivered);
    }
    match appserver::thread_state(server, &t.id) {
        Ok(appserver::ThreadState::Active) => return Ok(PushOutcome::Busy),
        Ok(appserver::ThreadState::Idle) => {}
        Err(e) => return Err(format!("cannot read thread status for {}: {e}", t.id)),
    }
    let kind = if auto_turn {
        OperationKind::WatchAutoTurn
    } else {
        OperationKind::WatchInject
    };
    let mut guard = lifecycle::admit_operation(&t.id, kind)?.into_guard()?;
    let drained = store::drain_with_guard(&mut guard)?;
    if drained.messages().is_empty() {
        return Ok(PushOutcome::Delivered);
    }
    let block = hook::mail_block(drained.messages(), &t.id);
    match appserver::deliver_with_guard(&mut guard, &block, auto_turn, settle_ms, t.allow_bus) {
        Ok(outcome) => {
            println!(
                "{}",
                str_obj(&[
                    ("delivered", &drained.messages().len().to_string()),
                    ("to", &t.id),
                    ("mode", if auto_turn { "auto-turn" } else { "inject" }),
                ])
            );
            match outcome {
                appserver::DeliveryOutcome::Delivered => Ok(PushOutcome::Delivered),
                appserver::DeliveryOutcome::AckDeferred => Ok(PushOutcome::AckDeferred(None)),
            }
        }
        Err(appserver::DeliveryError::BeforeInject(e)) => {
            drained.rollback()?;
            Err(format!("push to {} failed ({e}); mail re-enqueued", t.id))
        }
        Err(appserver::DeliveryError::AfterInject(e)) => Ok(PushOutcome::AckDeferred(Some(e))),
    }
}

fn wake_fallback(t: &Target, dry: bool) -> WakeOutcome {
    if dry {
        println!(
            "{}",
            str_obj(&[
                ("action", "wake-fallback"),
                ("id", &t.id),
                ("tool", &t.tool)
            ])
        );
        return WakeOutcome::Delivered;
    }
    let message = match DoorbellMessage::parse(DEFAULT_NUDGE) {
        Ok(message) => message,
        Err(error) => {
            eprintln!("[relay watch] invalid wake fallback message: {error}");
            return WakeOutcome::Failed;
        }
    };
    let mut guard = match lifecycle::admit_operation(&t.id, OperationKind::WatchWakeFallback)
        .and_then(lifecycle::Admission::into_guard)
    {
        Ok(guard) => guard,
        Err(error) => {
            eprintln!("[relay watch] wake fallback for {} refused: {error}", t.id);
            return WakeOutcome::Refused;
        }
    };
    let _resume_guard = match store::acquire_resume_lock(&t.id, &t.tool) {
        Ok(lock) => lock,
        Err(store::LockAcquireError::Busy(_)) => return WakeOutcome::Refused,
        Err(store::LockAcquireError::Io(error)) => {
            eprintln!(
                "[relay watch] wake fallback for {} cannot acquire resume lock: {error}",
                t.id
            );
            return WakeOutcome::Failed;
        }
    };
    match spawn::run_child_with_guard(&mut guard, ChildLaunchSpec::WatchWakeFallback(message)) {
        Ok(output) if output.status.success() => WakeOutcome::Delivered,
        Ok(output) if output.status.code() == Some(3) => WakeOutcome::Refused,
        Ok(output) => {
            eprintln!(
                "[relay watch] wake fallback for {} exited {}",
                t.id, output.status
            );
            WakeOutcome::Failed
        }
        Err(error) => {
            eprintln!("[relay watch] wake fallback for {} failed: {error}", t.id);
            WakeOutcome::Failed
        }
    }
}

fn str_obj(pairs: &[(&str, &str)]) -> String {
    let mut m: HashMap<String, JsonValue> = HashMap::new();
    for (k, v) in pairs {
        m.insert((*k).to_string(), JsonValue::from((*v).to_string()));
    }
    JsonValue::from(m)
        .stringify()
        .unwrap_or_else(|_| "{}".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decide_routes_only_codex_with_server_to_push() {
        assert_eq!(decide("codex", Some("/s.sock")), Mode::Push);
        assert_eq!(decide("codex", None), Mode::Wake);
        assert_eq!(decide("claude", Some("/s.sock")), Mode::Wake);
        assert_eq!(decide("claude", None), Mode::Wake);
    }

    #[test]
    fn follow_drops_an_overlong_incomplete_record_then_resumes() {
        let dir = std::env::temp_dir().join(format!("relay-follow-{}", store::uuid_v4()));
        std::fs::create_dir_all(&dir).expect("create fixture dir");
        let path = dir.join("mailbox.jsonl");
        std::fs::write(&path, vec![b'x'; MAX_FOLLOW_PENDING_BYTES + 1])
            .expect("write overlong record");

        let mut state = open_follow_file(&path, false).expect("open fixture");
        let mut out = Vec::new();
        read_follow_bytes(&mut state, &mut out).expect("read overlong record");
        assert!(state.dropping_overlong);
        assert!(state.pending.is_empty());
        assert!(out.is_empty());

        let mut append = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("reopen fixture");
        append.write_all(b"\nresumed\n").expect("finish fixture");
        read_follow_bytes(&mut state, &mut out).expect("resume after overlong record");
        assert!(!state.dropping_overlong);
        assert!(state.pending.is_empty());
        assert_eq!(out, b"resumed\n");

        std::fs::remove_file(path).expect("remove fixture file");
        std::fs::remove_dir(dir).expect("remove fixture dir");
    }
}
