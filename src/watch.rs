// Poll omp mailboxes and wake idle sessions; follow mode streams mailbox records.
use crate::cli::{Args, DEFAULT_NUDGE};
use crate::protocol::ProtocolStore;
use crate::sha256::Sha256;
use crate::store;
use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
use tinyjson::JsonValue;

const POLL_MS: u64 = 2000;
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

fn validate_tool(tool: &str) {
    if tool != "omp" {
        die(&format!("--tool must be omp, got: {tool}"));
    }
}

fn resolve_targets(args: &Args) -> Vec<Target> {
    if let Some(id) = args.flag("id") {
        if !store::is_uuid(id) {
            die(&format!("--id must be a session UUID, got: {id}"));
        }
        let tool = args.flag("tool").unwrap_or("omp").to_string();
        return vec![Target {
            id: id.to_string(),
            tool,
            dir: args.flag("dir").map(str::to_string),
        }];
    }
    if args.has("all") {
        return store::roster()
            .into_iter()
            .filter(|e| {
                let valid = store::is_uuid(&e.id);
                if !valid {
                    eprintln!("[relay watch] skip {}: not a session UUID", e.id);
                }
                valid
            })
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
            if !store::is_uuid(&e.id) {
                die(&format!("{who} is not a session UUID: {}", e.id));
            }
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
    if let Some(tool) = args.flag("tool") {
        validate_tool(tool);
    }
    if let Some(id) = args.flag("follow") {
        if !store::is_uuid(id) {
            die(&format!("--follow must be a session UUID, got: {id}"));
        }
        if args.has("all") || args.has("once") {
            die("--follow cannot be combined with --all or --once");
        }
        let tool = args.flag("tool").unwrap_or("omp");
        validate_tool(tool);
        let _guard = store::acquire_watcher_lock(id, tool, "follow")
            .unwrap_or_else(|e| die(&format!("cannot follow {id}: {e}")));
        follow_mailbox(id);
    }
    let once = args.has("once");
    let dry = args.has("dry");

    let targets = resolve_targets(&args);
    if targets.is_empty() {
        die(
            "usage: relay watch <nameOrId>... | --all | --id <uuid> [--dir <path>] [--tool omp] [--once] [--dry]",
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
        if let Err(error) = ProtocolStore::new(store::home_dir()).recover_pending() {
            eprintln!("[relay watch] protocol recovery failed: {error}");
            had_error = true;
            if once {
                std::process::exit(1);
            }
            std::thread::sleep(Duration::from_millis(POLL_MS));
            continue;
        }
        for t in &active_targets {
            if let Err(e) = store::update_watcher_progress(&t.id) {
                eprintln!("[relay watch] progress update for {} failed: {e}", t.id);
            }
            if !store::mailbox_has_content(&t.id) {
                woken.remove(&t.id);
                wake_retries.remove(&t.id);
                continue;
            }
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
        if let Err(e) = store::update_watcher_progress(id) {
            eprintln!("[relay watch] progress update for {id} failed: {e}");
        }
        std::thread::sleep(Duration::from_millis(POLL_MS));
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
    let Some(dir) = t.dir.as_deref().filter(|dir| Path::new(dir).is_dir()) else {
        eprintln!(
            "[relay watch] wake fallback for {} requires an existing directory",
            t.id
        );
        return WakeOutcome::Failed;
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
    let (cmd, args) = crate::cli::doorbell_args(&t.id, DEFAULT_NUDGE, None, None);
    match Command::new(cmd).args(args).current_dir(dir).status() {
        Ok(status) if status.success() => WakeOutcome::Delivered,
        Ok(status) if status.code() == Some(3) => WakeOutcome::Refused,
        Ok(status) => {
            eprintln!("[relay watch] wake fallback for {} exited {}", t.id, status);
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
