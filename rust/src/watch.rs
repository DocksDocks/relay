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
// the neutral doorbell nudge (never mail content — the guardian refuses
// actions instructed by untrusted mail). Targets that are not app-server
// reachable fall back to the wake doorbell. Mail safety: messages are drained
// atomically and re-enqueued if delivery fails.

use crate::cli::{Args, DEFAULT_NUDGE};
use crate::hook;
use crate::sha256::Sha256;
use crate::store;
use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tinyjson::JsonValue;

const POLL_MS: u64 = 2000;
const DEFAULT_SETTLE_MS: u64 = 5000;
const DEFAULT_TURN_WAIT_MS: u64 = 300_000;
const RPC_TIMEOUT_SECS: u64 = 20;
const WAKE_RETRY_MAX_MS: u64 = 30_000;

fn die(msg: &str) -> ! {
    eprintln!("{msg}");
    std::process::exit(1);
}

struct Target {
    id: String,
    tool: String,
    dir: Option<String>,
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
        }];
    }
    if args.has("all") {
        return store::roster()
            .into_iter()
            .filter(|e| server.is_none() || e.tool == "codex")
            .map(|e| Target {
                id: e.id,
                tool: e.tool,
                dir: e.dir,
            })
            .collect();
    }
    args.positionals(1)
        .iter()
        .map(|who| {
            let Some(e) = store::resolve(who) else {
                die(&format!("unknown session: {who}"));
            };
            Target {
                id: e.id,
                tool: args.flag("tool").map(str::to_string).unwrap_or(e.tool),
                dir: e.dir,
            }
        })
        .collect()
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
    let server = args.flag("server").map(str::to_string).or_else(|| {
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

    let targets = resolve_targets(&args, server.as_deref());
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
    let mut had_error = false;
    loop {
        for t in &active_targets {
            if let Err(e) = store::update_watcher_progress(&t.id) {
                eprintln!("[relay watch] progress update for {} failed: {e}", t.id);
            }
            let pending = store::peek(&t.id);
            if pending.is_empty() {
                woken.remove(&t.id);
                wake_retries.remove(&t.id);
                continue;
            }
            match decide(&t.tool, server.as_deref()) {
                Mode::Push => {
                    match push_target(server.as_deref().unwrap(), t, auto_turn, dry, settle_ms) {
                        Ok(_) => {}
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
                                "[relay watch] wake fallback for {} refused; retrying in {}ms",
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
    let mut added = Vec::new();
    state
        .file
        .read_to_end(&mut added)
        .map_err(|e| format!("read followed mailbox: {e}"))?;
    state.prefix_hash.update(&added);
    state.pending.extend_from_slice(&added);
    while let Some(end) = state.pending.iter().position(|b| *b == b'\n') {
        let line: Vec<u8> = state.pending.drain(..=end).collect();
        out.write_all(&line)
            .and_then(|()| out.flush())
            .map_err(|e| format!("write followed mailbox line: {e}"))?;
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
) -> Result<usize, String> {
    if dry {
        println!(
            "{}",
            str_obj(&[
                ("action", if auto_turn { "auto-turn" } else { "inject" }),
                ("id", &t.id),
                ("server", server),
            ])
        );
        return Ok(0);
    }
    let msgs = store::drain(&t.id)?;
    if msgs.is_empty() {
        return Ok(0);
    }
    let block = hook::mail_block(&msgs, &t.id);
    match deliver(server, &t.id, &block, auto_turn, settle_ms) {
        Ok(()) => {
            println!(
                "{}",
                str_obj(&[
                    ("delivered", &msgs.len().to_string()),
                    ("to", &t.id),
                    ("mode", if auto_turn { "auto-turn" } else { "inject" }),
                ])
            );
            Ok(msgs.len())
        }
        Err(e) => {
            for m in &msgs {
                if let Some(mo) = m.get::<HashMap<String, JsonValue>>() {
                    let _ = store::enqueue(&t.id, mo);
                }
            }
            Err(format!("push to {} failed ({e}); mail re-enqueued", t.id))
        }
    }
}

fn deliver(
    server: &str,
    thread_id: &str,
    block: &str,
    auto_turn: bool,
    settle_ms: u64,
) -> Result<(), String> {
    let mut ws = WsConn::connect(server)?;
    ws.request(
        0,
        "initialize",
        sobj(vec![(
            "clientInfo",
            sobj(vec![
                ("name", JsonValue::from("session-relay-watch".to_string())),
                ("title", JsonValue::from("session-relay watch".to_string())),
                (
                    "version",
                    JsonValue::from(env!("CARGO_PKG_VERSION").to_string()),
                ),
            ]),
        )]),
    )?;
    ws.notify("initialized", JsonValue::from(HashMap::new()))?;
    ws.request(
        1,
        "thread/resume",
        sobj(vec![("threadId", JsonValue::from(thread_id.to_string()))]),
    )?;
    ws.request(2, "thread/inject_items", inject_params(thread_id, block))?;
    if auto_turn {
        std::thread::sleep(Duration::from_millis(settle_ms));
        ws.request(3, "turn/start", turn_params(thread_id))?;
        // Stay attached: MCP tool calls elicit approval from the CONNECTED
        // client no matter the approvalPolicy; detaching here wedges the turn.
        let wait_ms: u64 = std::env::var("RELAY_TURN_WAIT_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_TURN_WAIT_MS);
        if !ws.pump_turn(wait_ms)? {
            eprintln!(
                "[relay watch] turn on {thread_id} still running after {wait_ms}ms — detaching (a later MCP call may wedge it)"
            );
        }
    }
    Ok(())
}

// Approve only the relay's own bus server (whoami/register/roster/send/inbox/
// discover — store-local, no shell, no file access); everything else is
// declined so the turn fails that call cleanly instead of wedging.
fn elicitation_action(server_name: &str) -> &'static str {
    if server_name == "bus" {
        "accept"
    } else {
        "decline"
    }
}

// Doorbell fallback via self-exec: `wake` lives in cli::run, which never
// returns, so reuse it as a child process rather than a call.
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
    let exe = std::env::current_exe().unwrap_or_else(|_| "relay".into());
    let mut c = std::process::Command::new(exe);
    c.arg("wake")
        .arg("--id")
        .arg(&t.id)
        .arg("--tool")
        .arg(&t.tool);
    if let Some(d) = &t.dir {
        c.arg("--dir").arg(d);
    }
    match c.status() {
        Ok(s) if s.success() => WakeOutcome::Delivered,
        Ok(s) if s.code() == Some(3) => WakeOutcome::Refused,
        Ok(s) => {
            eprintln!("[relay watch] wake fallback for {} exited {s}", t.id);
            WakeOutcome::Failed
        }
        Err(e) => {
            eprintln!(
                "[relay watch] wake fallback for {} failed to spawn: {e}",
                t.id
            );
            WakeOutcome::Failed
        }
    }
}

// ---- JSON-RPC param builders (pure — unit-tested shapes) ----

fn sobj(pairs: Vec<(&str, JsonValue)>) -> JsonValue {
    let mut m: HashMap<String, JsonValue> = HashMap::new();
    for (k, v) in pairs {
        m.insert(k.to_string(), v);
    }
    JsonValue::from(m)
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

fn inject_params(thread_id: &str, text: &str) -> JsonValue {
    sobj(vec![
        ("threadId", JsonValue::from(thread_id.to_string())),
        (
            "items",
            JsonValue::from(vec![sobj(vec![
                ("type", JsonValue::from("message".to_string())),
                ("role", JsonValue::from("user".to_string())),
                (
                    "content",
                    JsonValue::from(vec![sobj(vec![
                        ("type", JsonValue::from("input_text".to_string())),
                        ("text", JsonValue::from(text.to_string())),
                    ])]),
                ),
            ])]),
        ),
    ])
}

fn turn_params(thread_id: &str) -> JsonValue {
    sobj(vec![
        ("threadId", JsonValue::from(thread_id.to_string())),
        (
            "input",
            JsonValue::from(vec![sobj(vec![
                ("type", JsonValue::from("text".to_string())),
                ("text", JsonValue::from(DEFAULT_NUDGE.to_string())),
            ])]),
        ),
        ("approvalPolicy", JsonValue::from("never".to_string())),
    ])
}

// ---- minimal WebSocket client over a unix socket ----

fn urandom(n: usize) -> Vec<u8> {
    let mut f = std::fs::File::open("/dev/urandom")
        .unwrap_or_else(|e| die(&format!("open /dev/urandom: {e}")));
    let mut v = vec![0u8; n];
    f.read_exact(&mut v)
        .unwrap_or_else(|e| die(&format!("read /dev/urandom: {e}")));
    v
}

fn b64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn upgrade_request(key: &str) -> String {
    format!(
        "GET / HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
    )
}

// Client frames are always masked (RFC 6455 §5.3); server frames arrive
// unmasked but the parser tolerates either.
fn encode_client_frame(opcode: u8, payload: &[u8], mask: [u8; 4]) -> Vec<u8> {
    let mut f = vec![0x80 | (opcode & 0x0f)];
    let len = payload.len();
    if len < 126 {
        f.push(0x80 | len as u8);
    } else if len <= 0xFFFF {
        f.push(0x80 | 126);
        f.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        f.push(0x80 | 127);
        f.extend_from_slice(&(len as u64).to_be_bytes());
    }
    f.extend_from_slice(&mask);
    f.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    f
}

// (fin, opcode, payload, bytes consumed); None = incomplete frame, read more.
fn parse_frame(buf: &[u8]) -> Option<(bool, u8, Vec<u8>, usize)> {
    if buf.len() < 2 {
        return None;
    }
    let fin = buf[0] & 0x80 != 0;
    let opcode = buf[0] & 0x0f;
    let masked = buf[1] & 0x80 != 0;
    let mut len = (buf[1] & 0x7f) as usize;
    let mut i = 2;
    if len == 126 {
        if buf.len() < 4 {
            return None;
        }
        len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
        i = 4;
    } else if len == 127 {
        if buf.len() < 10 {
            return None;
        }
        len = u64::from_be_bytes(buf[2..10].try_into().ok()?) as usize;
        i = 10;
    }
    let mask: Option<[u8; 4]> = if masked {
        if buf.len() < i + 4 {
            return None;
        }
        let m = [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]];
        i += 4;
        Some(m)
    } else {
        None
    };
    if buf.len() < i + len {
        return None;
    }
    let mut payload = buf[i..i + len].to_vec();
    if let Some(m) = mask {
        for (k, b) in payload.iter_mut().enumerate() {
            *b ^= m[k % 4];
        }
    }
    Some((fin, opcode, payload, i + len))
}

struct WsConn {
    s: UnixStream,
    buf: Vec<u8>,
}

impl WsConn {
    fn connect(path: &str) -> Result<Self, String> {
        let mut s = UnixStream::connect(path).map_err(|e| format!("connect {path}: {e}"))?;
        s.set_read_timeout(Some(Duration::from_secs(10))).ok();
        let key = b64(&urandom(16));
        s.write_all(upgrade_request(&key).as_bytes())
            .map_err(|e| format!("upgrade write: {e}"))?;
        let mut hdr: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 512];
        let end = loop {
            if let Some(pos) = hdr.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos + 4;
            }
            if hdr.len() > 16384 {
                return Err("oversized upgrade response".into());
            }
            let n = s
                .read(&mut chunk)
                .map_err(|e| format!("upgrade read: {e}"))?;
            if n == 0 {
                return Err("connection closed during upgrade".into());
            }
            hdr.extend_from_slice(&chunk[..n]);
        };
        let head = String::from_utf8_lossy(&hdr[..end]);
        if !head.starts_with("HTTP/1.1 101") {
            return Err(format!(
                "upgrade refused: {}",
                head.lines().next().unwrap_or("")
            ));
        }
        Ok(WsConn {
            s,
            buf: hdr[end..].to_vec(),
        })
    }

    fn send_frame(&mut self, opcode: u8, payload: &[u8]) -> Result<(), String> {
        let mask: [u8; 4] = urandom(4).try_into().unwrap_or([0x5a; 4]);
        let frame = encode_client_frame(opcode, payload, mask);
        self.s
            .write_all(&frame)
            .map_err(|e| format!("ws write: {e}"))
    }

    // Ok(Some(text)) = one complete text message; Ok(None) = read timed out
    // (caller decides whether its own deadline has passed).
    fn recv_text(&mut self) -> Result<Option<String>, String> {
        let mut assembled: Vec<u8> = Vec::new();
        let mut in_text = false;
        loop {
            if let Some((fin, opcode, payload, used)) = parse_frame(&self.buf) {
                self.buf.drain(..used);
                match opcode {
                    0x1 => {
                        assembled = payload;
                        if fin {
                            return Ok(Some(String::from_utf8_lossy(&assembled).into_owned()));
                        }
                        in_text = true;
                    }
                    0x0 if in_text => {
                        assembled.extend_from_slice(&payload);
                        if fin {
                            return Ok(Some(String::from_utf8_lossy(&assembled).into_owned()));
                        }
                    }
                    0x9 => self.send_frame(0xA, &payload)?, // ping -> pong
                    0x8 => return Err("server closed the connection".into()),
                    _ => {} // pong / binary / stray continuation: ignore
                }
                continue;
            }
            let mut chunk = [0u8; 4096];
            match self.s.read(&mut chunk) {
                Ok(0) => return Err("connection closed".into()),
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    return Ok(None);
                }
                Err(e) => return Err(format!("ws read: {e}")),
            }
        }
    }

    fn notify(&mut self, method: &str, params: JsonValue) -> Result<(), String> {
        let msg = sobj(vec![
            ("method", JsonValue::from(method.to_string())),
            ("params", params),
        ]);
        self.send_frame(0x1, msg.stringify().map_err(|e| format!("{e}"))?.as_bytes())
    }

    // Send a request and pump messages until its response arrives; id-less
    // messages (event notifications) are ignored here.
    fn request(&mut self, id: u64, method: &str, params: JsonValue) -> Result<JsonValue, String> {
        let msg = sobj(vec![
            ("id", JsonValue::from(id as f64)),
            ("method", JsonValue::from(method.to_string())),
            ("params", params),
        ]);
        self.send_frame(0x1, msg.stringify().map_err(|e| format!("{e}"))?.as_bytes())?;
        let deadline = Instant::now() + Duration::from_secs(RPC_TIMEOUT_SECS);
        loop {
            if Instant::now() > deadline {
                return Err(format!("{method}: response timeout"));
            }
            let Some(text) = self.recv_text()? else {
                continue;
            };
            let Ok(v) = text.parse::<JsonValue>() else {
                continue;
            };
            let Some(o) = v.get::<HashMap<String, JsonValue>>() else {
                continue;
            };
            let got_id = o.get("id").and_then(|x| x.get::<f64>().copied());
            if got_id != Some(id as f64) {
                continue;
            }
            if let Some(err) = o.get("error") {
                let emsg = err
                    .get::<HashMap<String, JsonValue>>()
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.get::<String>().cloned())
                    .unwrap_or_else(|| "unknown error".into());
                return Err(format!("{method}: {emsg}"));
            }
            return Ok(o.get("result").cloned().unwrap_or(JsonValue::from(())));
        }
    }

    // Pump the event stream until the turn ends. Ok(true) = turn/completed or
    // turn/failed seen; Ok(false) = still running at the deadline. Server->
    // client `mcpServer/elicitation/request`s are answered per
    // `elicitation_action`; other server requests are left alone (unknown
    // response schemas — answering blind is worse than the caller's timeout).
    fn pump_turn(&mut self, ms: u64) -> Result<bool, String> {
        let deadline = Instant::now() + Duration::from_millis(ms);
        loop {
            if Instant::now() > deadline {
                return Ok(false);
            }
            let Some(text) = self.recv_text()? else {
                continue;
            };
            let Ok(v) = text.parse::<JsonValue>() else {
                continue;
            };
            let Some(o) = v.get::<HashMap<String, JsonValue>>() else {
                continue;
            };
            let method = o.get("method").and_then(|m| m.get::<String>().cloned());
            match method.as_deref() {
                Some("turn/completed") | Some("turn/failed") => return Ok(true),
                Some("mcpServer/elicitation/request") => {
                    let Some(req_id) = o.get("id").cloned() else {
                        continue; // notification form: nothing to answer
                    };
                    let server_name = o
                        .get("params")
                        .and_then(|p| p.get::<HashMap<String, JsonValue>>())
                        .and_then(|p| p.get("serverName"))
                        .and_then(|s| s.get::<String>().cloned())
                        .unwrap_or_default();
                    let action = elicitation_action(&server_name);
                    let msg = sobj(vec![
                        ("id", req_id),
                        (
                            "result",
                            sobj(vec![("action", JsonValue::from(action.to_string()))]),
                        ),
                    ]);
                    self.send_frame(0x1, msg.stringify().map_err(|e| format!("{e}"))?.as_bytes())?;
                }
                _ => {}
            }
        }
    }
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
    fn b64_matches_known_vectors() {
        assert_eq!(b64(b"abc"), "YWJj");
        assert_eq!(b64(b"ab"), "YWI=");
        assert_eq!(b64(b"a"), "YQ==");
        assert_eq!(b64(&[0u8; 16]).len(), 24); // Sec-WebSocket-Key length
    }

    #[test]
    fn frame_roundtrip_small_and_extended_lengths() {
        for payload in [&b"hello"[..], &[0x42u8; 300][..]] {
            let f = encode_client_frame(0x1, payload, [1, 2, 3, 4]);
            let (fin, opcode, decoded, used) = parse_frame(&f).expect("complete frame");
            assert!(fin);
            assert_eq!(opcode, 0x1);
            assert_eq!(decoded, payload);
            assert_eq!(used, f.len());
        }
        assert!(parse_frame(&[0x81]).is_none()); // incomplete header
    }

    #[test]
    fn elicitations_accept_only_the_relay_bus_server() {
        assert_eq!(elicitation_action("bus"), "accept");
        assert_eq!(elicitation_action("codex_apps"), "decline");
        assert_eq!(elicitation_action(""), "decline");
    }

    #[test]
    fn upgrade_request_carries_the_ws_headers() {
        let r = upgrade_request("KEY123");
        assert!(r.contains("Upgrade: websocket"));
        assert!(r.contains("Sec-WebSocket-Key: KEY123"));
        assert!(r.contains("Sec-WebSocket-Version: 13"));
        assert!(r.ends_with("\r\n\r\n"));
    }

    fn as_obj(v: &JsonValue) -> &HashMap<String, JsonValue> {
        v.get::<HashMap<String, JsonValue>>().expect("object")
    }

    #[test]
    fn inject_params_wrap_the_fenced_text_as_a_user_input_item() {
        let p = inject_params("tid-1", "<session-relay-mail>hi</session-relay-mail>");
        let o = as_obj(&p);
        assert_eq!(o.get("threadId").unwrap().get::<String>().unwrap(), "tid-1");
        let items = o.get("items").unwrap().get::<Vec<JsonValue>>().unwrap();
        let item = as_obj(&items[0]);
        assert_eq!(item.get("role").unwrap().get::<String>().unwrap(), "user");
        let content = item
            .get("content")
            .unwrap()
            .get::<Vec<JsonValue>>()
            .unwrap();
        let c0 = as_obj(&content[0]);
        assert_eq!(
            c0.get("type").unwrap().get::<String>().unwrap(),
            "input_text"
        );
        assert!(
            c0.get("text")
                .unwrap()
                .get::<String>()
                .unwrap()
                .contains("session-relay-mail")
        );
    }

    #[test]
    fn turn_params_use_the_neutral_nudge_and_never_approvals() {
        let p = turn_params("tid-2");
        let o = as_obj(&p);
        assert_eq!(
            o.get("approvalPolicy").unwrap().get::<String>().unwrap(),
            "never"
        );
        let input = o.get("input").unwrap().get::<Vec<JsonValue>>().unwrap();
        let text = as_obj(&input[0])
            .get("text")
            .unwrap()
            .get::<String>()
            .cloned()
            .unwrap();
        assert_eq!(text, DEFAULT_NUDGE);
        assert!(!text.contains("session-relay-mail")); // nudge, never mail content
    }
}
