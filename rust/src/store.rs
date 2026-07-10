// store.rs — shared on-disk state for the session-relay bus (port of lib/store.mjs).
// Holds three things, all under one fixed home so every component agrees:
//   registry.json      id -> { id, dir, name, tool, lastSeen } + a name -> id index
//   mailbox/<id>.jsonl one append-only inbox per recipient session id
//   markers/<cwd>      the session id last registered for a project dir
//
// Home is a FIXED, TOOL-NEUTRAL path (~/.agent-relay, never under the plugin
// root — the install dir is replaced on every plugin update). Override with
// AGENT_RELAY_HOME; SESSION_RELAY_HOME is a back-compat alias.
//
// Cross-process safety: every mutation runs under a kernel flock(2) on
// <home>/.lock (rustix; auto-released on crash — no stale-reclaim dance).
// The v1 Node store used a mkdir-mutex where `.lock` was a DIRECTORY; a
// leftover dir is migrated (removed) on first lock acquisition. Registry and
// marker writes are atomic (tmp + rename); mailbox appends serialize under
// the same lock.

use rustix::fs::{FlockOperation, flock};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tinyjson::JsonValue;

const WATCH_LOCK_RETRY: Duration = Duration::from_secs(2);
const DEFAULT_GC_DAYS: u64 = 14;
const GC_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const SECONDS_PER_DAY: u64 = 24 * 60 * 60;
pub const WATCH_PROGRESS_STALE_MS: i64 = 300_000;

fn home_override() -> Option<PathBuf> {
    for var in ["AGENT_RELAY_HOME", "SESSION_RELAY_HOME"] {
        if let Ok(v) = std::env::var(var) {
            if !v.is_empty() {
                return Some(PathBuf::from(v));
            }
        }
    }
    None
}

pub fn home_dir() -> PathBuf {
    if let Some(path) = home_override() {
        return path;
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    Path::new(&home).join(".agent-relay")
}

fn registry_path() -> PathBuf {
    home_dir().join("registry.json")
}
pub(crate) fn mailbox_path(id: &str) -> PathBuf {
    home_dir()
        .join("mailbox")
        .join(format!("{}.jsonl", sanitize(id)))
}
fn marker_path(dir: &str) -> PathBuf {
    home_dir().join("markers").join(encode_dir(dir))
}
fn lock_path() -> PathBuf {
    home_dir().join(".lock")
}

pub fn watcher_lock_path(id: &str) -> PathBuf {
    home_dir()
        .join("watchers")
        .join(format!("{}.lock", sanitize(id)))
}

pub fn watcher_progress_path(id: &str) -> PathBuf {
    home_dir()
        .join("watchers")
        .join(format!("{}.progress", sanitize(id)))
}

pub fn resume_lock_path(id: &str) -> PathBuf {
    home_dir()
        .join("locks")
        .join(format!("resume-{}.lock", sanitize(id)))
}

/// Filesystem-safe key for a project dir — mirrors Claude Code's own scheme
/// (every non-alphanumeric char becomes '-').
pub fn encode_dir(dir: &str) -> String {
    let abs = std::path::absolute(dir).unwrap_or_else(|_| PathBuf::from(dir));
    abs.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Path-traversal defense for recipient ids used as mailbox filenames.
pub fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

fn ensure_dirs() -> Result<(), String> {
    for d in ["mailbox", "markers", "watchers", "locks"] {
        let path = home_dir().join(d);
        fs::create_dir_all(&path).map_err(|e| format!("mkdir {d}: {e}"))?;
        if matches!(d, "watchers" | "locks") {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
                .map_err(|e| format!("chmod {}: {e}", path.display()))?;
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LockStatus {
    Never,
    Live,
    Dead,
    Unknown,
}

impl LockStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Never => "never",
            Self::Live => "live",
            Self::Dead => "dead",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LockMetadata {
    pub pid: u32,
    pub started_at: String,
    pub tool: String,
    pub mode: String,
}

impl LockMetadata {
    fn new(tool: &str, mode: &str) -> Self {
        Self {
            pid: std::process::id(),
            started_at: iso_now(),
            tool: tool.to_string(),
            mode: mode.to_string(),
        }
    }

    fn to_json(&self) -> JsonValue {
        let mut m = HashMap::new();
        m.insert("pid".to_string(), JsonValue::from(self.pid as f64));
        m.insert(
            "started_at".to_string(),
            JsonValue::from(self.started_at.clone()),
        );
        m.insert("tool".to_string(), JsonValue::from(self.tool.clone()));
        m.insert("mode".to_string(), JsonValue::from(self.mode.clone()));
        JsonValue::from(m)
    }

    fn from_json(value: &JsonValue) -> Option<Self> {
        let o = value.get::<HashMap<String, JsonValue>>()?;
        let pid = o.get("pid")?.get::<f64>().copied()?;
        let string = |key: &str| o.get(key)?.get::<String>().cloned();
        Some(Self {
            pid: (pid.is_finite() && pid >= 0.0 && pid <= u32::MAX as f64).then_some(pid as u32)?,
            started_at: string("started_at")?,
            tool: string("tool")?,
            mode: string("mode")?,
        })
    }
}

pub struct HeldLock {
    _file: fs::File,
    metadata: LockMetadata,
}

impl HeldLock {
    pub fn metadata(&self) -> &LockMetadata {
        &self.metadata
    }
}

#[derive(Debug)]
pub enum LockAcquireError {
    Busy(Option<LockMetadata>),
    Io(String),
}

impl std::fmt::Display for LockAcquireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy(_) => write!(f, "lock already held"),
            Self::Io(e) => f.write_str(e),
        }
    }
}

fn open_lock(path: &Path, create: bool) -> Result<fs::File, std::io::Error> {
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).mode(0o600);
    if create {
        options.create(true);
    }
    options.open(path)
}

fn read_lock_metadata_file(file: &mut fs::File) -> Option<LockMetadata> {
    let mut raw = String::new();
    file.seek(SeekFrom::Start(0)).ok()?;
    file.read_to_string(&mut raw).ok()?;
    LockMetadata::from_json(&raw.parse().ok()?)
}

pub fn read_lock_metadata(path: &Path) -> Option<LockMetadata> {
    let mut file = open_lock(path, false).ok()?;
    read_lock_metadata_file(&mut file)
}

fn acquire_lock(
    path: &Path,
    metadata: LockMetadata,
    retry_for: Duration,
) -> Result<HeldLock, LockAcquireError> {
    let deadline = Instant::now() + retry_for;
    loop {
        // Coordinate creation/acquisition with GC's global store lock. Once
        // this physical lock is acquired the returned fd keeps it live after
        // the global lock is released.
        let attempt = with_lock(|| {
            let mut file =
                open_lock(path, true).map_err(|e| format!("open lock {}: {e}", path.display()))?;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))
                .map_err(|e| format!("chmod {}: {e}", path.display()))?;
            match flock(&file, FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => {}
                Err(e) if e == rustix::io::Errno::AGAIN || e == rustix::io::Errno::INTR => {
                    return Ok(Err(read_lock_metadata_file(&mut file)));
                }
                Err(e) => return Err(format!("flock {}: {e}", path.display())),
            }
            let encoded = metadata
                .to_json()
                .stringify()
                .map_err(|e| format!("lock metadata serialize: {e}"))?;
            file.set_len(0)
                .and_then(|()| file.seek(SeekFrom::Start(0)).map(|_| ()))
                .and_then(|()| file.write_all(encoded.as_bytes()))
                .and_then(|()| file.flush())
                .map_err(|e| format!("write lock metadata {}: {e}", path.display()))?;
            Ok(Ok(HeldLock {
                _file: file,
                metadata: metadata.clone(),
            }))
        })
        .map_err(LockAcquireError::Io)?;
        match attempt {
            Ok(guard) => return Ok(guard),
            Err(holder) if Instant::now() >= deadline => {
                return Err(LockAcquireError::Busy(holder));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(25)),
        }
    }
}

pub fn acquire_watcher_lock(
    id: &str,
    tool: &str,
    mode: &str,
) -> Result<HeldLock, LockAcquireError> {
    acquire_lock(
        &watcher_lock_path(id),
        LockMetadata::new(tool, mode),
        WATCH_LOCK_RETRY,
    )
}

pub fn acquire_resume_lock(id: &str, tool: &str) -> Result<HeldLock, LockAcquireError> {
    acquire_lock(
        &resume_lock_path(id),
        LockMetadata::new(tool, "resume"),
        Duration::ZERO,
    )
}

pub fn lock_status(path: &Path) -> LockStatus {
    let file = match open_lock(path, false) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return LockStatus::Never,
        Err(_) => return LockStatus::Unknown,
    };
    match flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {
            let _ = flock(&file, FlockOperation::Unlock);
            LockStatus::Dead
        }
        Err(e) if e == rustix::io::Errno::AGAIN => LockStatus::Live,
        Err(_) => LockStatus::Unknown,
    }
}

pub fn watcher_status(id: &str) -> LockStatus {
    lock_status(&watcher_lock_path(id))
}

pub fn resume_status(id: &str) -> LockStatus {
    lock_status(&resume_lock_path(id))
}

pub fn update_watcher_progress(id: &str) -> Result<(), String> {
    ensure_dirs()?;
    let path = watcher_progress_path(id);
    atomic_write(&path, &format!("{}\n", now_ms()))?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("chmod {}: {e}", path.display()))
}

pub fn watcher_progress_age_ms(id: &str) -> Option<i64> {
    let raw = fs::read_to_string(watcher_progress_path(id)).ok()?;
    let written: i64 = raw.trim().parse().ok()?;
    Some(now_ms().saturating_sub(written).max(0))
}

fn random_hex(n_bytes: usize) -> String {
    let mut buf = vec![0u8; n_bytes];
    // /dev/urandom exists on every unix target we ship (linux-musl, apple-darwin);
    // rustix::rand::getrandom is Linux-only, so std + the device file it is.
    let mut f = fs::File::open("/dev/urandom").expect("open /dev/urandom");
    f.read_exact(&mut buf).expect("read /dev/urandom");
    use std::fmt::Write as _;
    buf.iter()
        .fold(String::with_capacity(n_bytes * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

pub fn uuid_v4() -> String {
    let mut b = vec![0u8; 16];
    let mut f = fs::File::open("/dev/urandom").expect("open /dev/urandom");
    f.read_exact(&mut b).expect("read /dev/urandom");
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0],
        b[1],
        b[2],
        b[3],
        b[4],
        b[5],
        b[6],
        b[7],
        b[8],
        b[9],
        b[10],
        b[11],
        b[12],
        b[13],
        b[14],
        b[15]
    )
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis() as i64
}

/// ISO-8601 UTC with millisecond precision — matches Node's Date#toISOString.
pub fn iso_from_unix_ms(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let millis = ms.rem_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, mo, da) = civil_from_days(days);
    format!("{y:04}-{mo:02}-{da:02}T{h:02}:{mi:02}:{s:02}.{millis:03}Z")
}

pub fn iso_now() -> String {
    iso_from_unix_ms(now_ms())
}

/// Session ids must be UUID-shaped — both tools mint UUIDs, so a non-UUID id
/// is a planted/garbage value (and this keeps ids off doorbell argv as
/// injectable options). Mirrors the Node UUID_RE (case-insensitive).
pub fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// Days-since-epoch -> (year, month, day). Howard Hinnant's civil_from_days.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn atomic_write(file: &Path, text: &str) -> Result<(), String> {
    let tmp = PathBuf::from(format!(
        "{}.{}.{}.tmp",
        file.display(),
        std::process::id(),
        random_hex(4)
    ));
    fs::write(&tmp, text).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    fs::rename(&tmp, file).map_err(|e| format!("rename to {}: {e}", file.display()))
}

/// Run `f` holding an exclusive kernel flock on <home>/.lock.
/// Fail-fast contract preserved from the Node store: give up after 3s of
/// live contention. Crashed holders need no reclaim — the kernel releases.
pub fn with_lock<T>(f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    ensure_dirs()?;
    let lock = lock_path();
    // Migration: the v1 mkdir-mutex left `.lock` as a DIRECTORY. After the
    // one-commit flip every store toucher is this binary, so a dir here is by
    // definition abandoned — remove it so open() below doesn't fail EISDIR.
    if let Ok(md) = fs::metadata(&lock) {
        if md.is_dir() {
            let _ = fs::remove_dir(&lock);
        }
    }
    let file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false) // a lock file's content is irrelevant; never clobber it
        .open(&lock)
        .map_err(|e| format!("open lock {}: {e}", lock.display()))?;
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => break,
            Err(e) if e == rustix::io::Errno::AGAIN || e == rustix::io::Errno::INTR => {
                if Instant::now() > deadline {
                    return Err("session-relay: lock busy (held > 3s)".to_string());
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => return Err(format!("flock {}: {e}", lock.display())),
        }
    }
    let out = f();
    let _ = flock(&file, FlockOperation::Unlock); // belt; close releases anyway
    out
}

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub id: String,
    pub dir: Option<String>,
    pub name: Option<String>,
    pub tool: String,
    pub last_seen: String,
}

impl Entry {
    pub fn to_json(&self) -> JsonValue {
        let mut m: HashMap<String, JsonValue> = HashMap::new();
        m.insert("id".into(), JsonValue::from(self.id.clone()));
        m.insert(
            "dir".into(),
            self.dir
                .clone()
                .map(JsonValue::from)
                .unwrap_or(JsonValue::from(())),
        );
        m.insert(
            "name".into(),
            self.name
                .clone()
                .map(JsonValue::from)
                .unwrap_or(JsonValue::from(())),
        );
        m.insert("tool".into(), JsonValue::from(self.tool.clone()));
        m.insert("lastSeen".into(), JsonValue::from(self.last_seen.clone()));
        JsonValue::from(m)
    }

    fn from_json(v: &JsonValue) -> Option<Entry> {
        let obj: &HashMap<String, JsonValue> = v.get()?;
        let s = |k: &str| -> Option<String> { obj.get(k)?.get::<String>().cloned() };
        Some(Entry {
            id: s("id")?,
            dir: s("dir"),
            name: s("name"),
            tool: s("tool").unwrap_or_else(|| "claude".to_string()),
            last_seen: s("lastSeen").unwrap_or_default(),
        })
    }
}

type Registry = (HashMap<String, JsonValue>, HashMap<String, JsonValue>);

// Any read/parse failure yields an empty registry — mirrors readJSON(file, fallback).
fn read_registry() -> Registry {
    let mut agents = HashMap::new();
    let mut names = HashMap::new();
    if let Ok(raw) = fs::read_to_string(registry_path()) {
        if let Ok(v) = raw.parse::<JsonValue>() {
            if let Some(obj) = v.get::<HashMap<String, JsonValue>>() {
                if let Some(a) = obj
                    .get("agents")
                    .and_then(|x| x.get::<HashMap<String, JsonValue>>())
                {
                    agents = a.clone();
                }
                if let Some(n) = obj
                    .get("names")
                    .and_then(|x| x.get::<HashMap<String, JsonValue>>())
                {
                    names = n.clone();
                }
            }
        }
    }
    (agents, names)
}

fn write_registry(
    agents: HashMap<String, JsonValue>,
    names: HashMap<String, JsonValue>,
) -> Result<(), String> {
    let mut root: HashMap<String, JsonValue> = HashMap::new();
    root.insert("agents".into(), JsonValue::from(agents));
    root.insert("names".into(), JsonValue::from(names));
    let text = JsonValue::from(root)
        .format() // pretty, 2-space — same shape JSON.stringify(reg, null, 2) wrote
        .map_err(|e| format!("registry serialize: {e}"))?;
    atomic_write(&registry_path(), &text)
}

#[derive(Default)]
struct GcCandidate {
    id: Option<String>,
    registry_key: Option<String>,
    last_seen: Option<String>,
    paths: Vec<PathBuf>,
}

fn gc_days() -> Result<u64, String> {
    match std::env::var("AGENT_RELAY_GC_DAYS") {
        Ok(raw) if !raw.is_empty() => raw
            .parse()
            .map_err(|_| "AGENT_RELAY_GC_DAYS must be a non-negative integer".to_string()),
        Ok(_) | Err(std::env::VarError::NotPresent) => Ok(DEFAULT_GC_DAYS),
        Err(std::env::VarError::NotUnicode(_)) => {
            Err("AGENT_RELAY_GC_DAYS must be valid UTF-8".to_string())
        }
    }
}

/// Resolve an existing store without creating anything. An explicit relay-home
/// override is the authority for its own root (including test roots in /tmp);
/// the default root must resolve beneath the configured HOME.
fn safe_existing_root() -> Result<Option<(PathBuf, PathBuf)>, String> {
    let raw =
        std::path::absolute(home_dir()).map_err(|e| format!("resolve relay store root: {e}"))?;
    match fs::symlink_metadata(&raw) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("stat relay store root {}: {e}", raw.display())),
    }
    let resolved = fs::canonicalize(&raw)
        .map_err(|e| format!("canonicalize relay store root {}: {e}", raw.display()))?;
    if !resolved.is_dir() {
        return Err(format!(
            "refusing GC: relay store root is not a directory: {}",
            resolved.display()
        ));
    }
    if home_override().is_none() {
        let home = std::env::var("HOME").map_err(|_| {
            "refusing GC: HOME is unavailable for default-root validation".to_string()
        })?;
        let home =
            fs::canonicalize(&home).map_err(|e| format!("canonicalize HOME for relay GC: {e}"))?;
        if resolved == home || !resolved.starts_with(&home) {
            return Err(format!(
                "refusing GC: default relay store {} resolves outside HOME {}",
                resolved.display(),
                home.display()
            ));
        }
    }
    Ok(Some((raw, resolved)))
}

fn add_gc_surface(
    candidates: &mut HashMap<String, GcCandidate>,
    id: Option<String>,
    orphan_key: String,
    path: PathBuf,
) {
    let key = id
        .as_ref()
        .map(|id| format!("session:{id}"))
        .unwrap_or(orphan_key);
    let candidate = candidates.entry(key).or_default();
    if candidate.id.is_none() {
        candidate.id = id;
    }
    candidate.paths.push(path);
}

fn known_gc_surfaces(root: &Path) -> Result<Vec<(Option<String>, String, PathBuf)>, String> {
    let mut surfaces = Vec::new();
    for directory in ["mailbox", "markers", "watchers", "locks", "spawn-logs"] {
        let dir = root.join(directory);
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("read GC surface {}: {e}", dir.display())),
        };
        for entry in entries {
            let entry = entry.map_err(|e| format!("read GC surface {}: {e}", dir.display()))?;
            let path = entry.path();
            if fs::symlink_metadata(&path)
                .map(|metadata| metadata.is_dir())
                .unwrap_or(false)
            {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let id = match directory {
                "mailbox" => name.strip_suffix(".jsonl").map(str::to_string),
                "markers" => fs::read_to_string(&path)
                    .ok()
                    .map(|raw| raw.trim().to_string())
                    .filter(|id| !id.is_empty()),
                "watchers" => name
                    .strip_suffix(".lock")
                    .or_else(|| name.strip_suffix(".progress"))
                    .map(str::to_string),
                "locks" => name
                    .strip_prefix("resume-")
                    .and_then(|name| name.strip_suffix(".lock"))
                    .map(str::to_string),
                "spawn-logs" => name.strip_suffix(".stderr").map(str::to_string),
                _ => None,
            };
            let known_name = id.is_some() || directory == "markers";
            if !known_name {
                continue;
            }
            surfaces.push((id, format!("orphan:{directory}:{}", path.display()), path));
        }
    }
    Ok(surfaces)
}

fn validate_gc_path(root: &Path, resolved_root: &Path, path: &Path) -> Result<(), String> {
    let allowed_parent = ["mailbox", "markers", "watchers", "locks", "spawn-logs"]
        .iter()
        .map(|directory| root.join(directory))
        .any(|directory| path.parent() == Some(directory.as_path()));
    if path == root || !path.starts_with(root) || !allowed_parent {
        return Err(format!(
            "refusing GC: path is not a known relay-owned surface: {}",
            path.display()
        ));
    }
    let resolved = fs::canonicalize(path)
        .map_err(|e| format!("canonicalize GC path {}: {e}", path.display()))?;
    if resolved == resolved_root || !resolved.starts_with(resolved_root) {
        return Err(format!(
            "refusing GC: path resolves outside relay store: {} -> {}",
            path.display(),
            resolved.display()
        ));
    }
    Ok(())
}

fn surface_is_old(path: &Path, cutoff: SystemTime) -> bool {
    fs::symlink_metadata(path)
        .and_then(|metadata| metadata.modified())
        .map(|modified| modified <= cutoff)
        .unwrap_or(false)
}

/// Opportunistically remove sessions whose every known surface is older than
/// the configured threshold. `self_id` is belt-and-braces: a shared-dir bus may
/// resolve the marker owner's id rather than this process's id, so fresh
/// last_seen/mtimes and held locks remain the primary liveness protections.
/// `None` means identity unknown, not "skip GC".
pub fn gc(now: SystemTime, self_id: Option<&str>) -> Result<usize, String> {
    let days = gc_days()?;
    if days == 0 {
        return Ok(0);
    }
    let Some((root, resolved_root)) = safe_existing_root()? else {
        return Ok(0);
    };
    let age = Duration::from_secs(
        days.checked_mul(SECONDS_PER_DAY)
            .ok_or_else(|| "AGENT_RELAY_GC_DAYS is too large".to_string())?,
    );
    let cutoff = now.checked_sub(age).unwrap_or(UNIX_EPOCH);
    let cutoff_ms = cutoff
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis()
        .min(i64::MAX as u128) as i64;
    let cutoff_iso = iso_from_unix_ms(cutoff_ms);

    with_lock(|| {
        // Re-check after acquiring the mutation lock. This also catches a
        // store-root symlink swap before any deletion begins.
        let Some((locked_root, locked_resolved_root)) = safe_existing_root()? else {
            return Ok(0);
        };
        if locked_root != root || locked_resolved_root != resolved_root {
            return Err("refusing GC: relay store root changed during sweep".to_string());
        }
        let stamp = root.join("gc-stamp");
        if let Ok(modified) = fs::metadata(&stamp).and_then(|metadata| metadata.modified()) {
            if now.duration_since(modified).unwrap_or(Duration::ZERO) < GC_INTERVAL {
                return Ok(0);
            }
        }

        let (mut agents, mut names) = read_registry();
        let mut candidates: HashMap<String, GcCandidate> = HashMap::new();
        for (registry_key, value) in &agents {
            let Some(entry) = Entry::from_json(value) else {
                continue; // malformed registry state is preserved, never guessed old
            };
            let candidate = candidates
                .entry(format!("session:{registry_key}"))
                .or_default();
            candidate.id = Some(registry_key.clone());
            candidate.registry_key = Some(registry_key.clone());
            candidate.last_seen = Some(entry.last_seen);
        }
        for (id, orphan_key, path) in known_gc_surfaces(&root)? {
            add_gc_surface(&mut candidates, id, orphan_key, path);
        }

        // Validate the complete deletion set before mutating any surface.
        for candidate in candidates.values() {
            for path in &candidate.paths {
                validate_gc_path(&root, &resolved_root, path)?;
            }
        }

        let mut eligible = Vec::new();
        for (key, candidate) in &candidates {
            if candidate
                .id
                .as_deref()
                .is_some_and(|id| Some(id) == self_id)
            {
                continue;
            }
            if candidate
                .last_seen
                .as_ref()
                .is_some_and(|last_seen| last_seen.is_empty() || last_seen > &cutoff_iso)
            {
                continue;
            }
            if !candidate
                .paths
                .iter()
                .all(|path| surface_is_old(path, cutoff))
            {
                continue;
            }
            if let Some(id) = candidate.id.as_deref() {
                if matches!(watcher_status(id), LockStatus::Live | LockStatus::Unknown)
                    || matches!(resume_status(id), LockStatus::Live | LockStatus::Unknown)
                {
                    continue;
                }
            }
            eligible.push(key.clone());
        }

        let mut removed_files = 0;
        let mut removed_registry_ids = HashSet::new();
        for key in &eligible {
            let candidate = &candidates[key];
            for path in &candidate.paths {
                match fs::remove_file(path) {
                    Ok(()) => removed_files += 1,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(format!("remove GC surface {}: {e}", path.display())),
                }
            }
            if let Some(registry_key) = &candidate.registry_key {
                removed_registry_ids.insert(registry_key.clone());
            }
        }

        // Registry is last: an interrupted sweep leaves visible entries that
        // a later pass can retry, never invisible orphan files.
        if !removed_registry_ids.is_empty() {
            for id in &removed_registry_ids {
                agents.remove(id);
            }
            names.retain(|_, value| {
                value
                    .get::<String>()
                    .is_none_or(|id| !removed_registry_ids.contains(id))
            });
            write_registry(agents, names)?;
        }
        atomic_write(&stamp, &format!("{}\n", iso_now()))?;
        Ok(removed_files + removed_registry_ids.len())
    })
}

/// Upsert a session. Missing fields are preserved from any prior entry, so the
/// hook (id + dir, no name) and a later register(name) compose cleanly.
pub fn register(
    id: &str,
    dir: Option<&str>,
    name: Option<&str>,
    tool: Option<&str>,
) -> Result<Entry, String> {
    if id.is_empty() {
        return Err("register requires an id".to_string());
    }
    with_lock(|| {
        let (mut agents, mut names) = read_registry();
        let prev = agents.get(id).and_then(Entry::from_json);
        let entry = Entry {
            id: id.to_string(),
            dir: dir
                .map(|d| {
                    std::path::absolute(d)
                        .unwrap_or_else(|_| PathBuf::from(d))
                        .to_string_lossy()
                        .into_owned()
                })
                .or_else(|| prev.as_ref().and_then(|p| p.dir.clone())),
            name: name
                .map(str::to_string)
                .or_else(|| prev.as_ref().and_then(|p| p.name.clone())),
            tool: tool
                .map(str::to_string)
                .or_else(|| prev.as_ref().map(|p| p.tool.clone()))
                .unwrap_or_else(|| "claude".to_string()),
            last_seen: iso_now(),
        };
        agents.insert(id.to_string(), entry.to_json());
        if let Some(n) = &entry.name {
            // drop a renamed alias: any other name bound to this id
            names.retain(|k, v| !(v.get::<String>().map(|s| s == id).unwrap_or(false) && k != n));
            names.insert(n.clone(), JsonValue::from(id.to_string()));
        }
        write_registry(agents, names)?;
        Ok(entry)
    })
}

pub fn roster() -> Vec<Entry> {
    let (agents, _) = read_registry();
    let mut out: Vec<Entry> = agents.values().filter_map(Entry::from_json).collect();
    out.sort_by(|a, b| {
        let ka = a.name.as_deref().unwrap_or(&a.id);
        let kb = b.name.as_deref().unwrap_or(&b.id);
        ka.cmp(kb)
    });
    out
}

/// Resolve a target given either a friendly name or a raw session id.
pub fn resolve(name_or_id: &str) -> Option<Entry> {
    if name_or_id.is_empty() {
        return None;
    }
    let (agents, names) = read_registry();
    if let Some(e) = agents.get(name_or_id).and_then(Entry::from_json) {
        return Some(e);
    }
    let id = names.get(name_or_id)?.get::<String>()?.clone();
    agents.get(&id).and_then(Entry::from_json)
}

pub fn set_marker(dir: &str, id: &str) -> Result<(), String> {
    with_lock(|| atomic_write(&marker_path(dir), &format!("{id}\n")))
}

pub fn id_for_dir(dir: &str) -> Option<String> {
    let raw = fs::read_to_string(marker_path(dir)).ok()?;
    let t = raw.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// Append one message. Generated id/ts can be overridden by keys in `msg`
/// (same semantics as the JS object spread in the Node store).
pub fn enqueue(recipient_id: &str, msg: &HashMap<String, JsonValue>) -> Result<(), String> {
    with_lock(|| {
        let mut m: HashMap<String, JsonValue> = HashMap::new();
        m.insert("id".into(), JsonValue::from(uuid_v4()));
        m.insert("ts".into(), JsonValue::from(iso_now()));
        for (k, v) in msg {
            m.insert(k.clone(), v.clone());
        }
        let line = JsonValue::from(m)
            .stringify()
            .map_err(|e| format!("message serialize: {e}"))?;
        let path = mailbox_path(recipient_id);
        use std::io::Write;
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| format!("open mailbox {}: {e}", path.display()))?;
        f.write_all(format!("{line}\n").as_bytes())
            .map_err(|e| format!("append mailbox: {e}"))?;
        Ok(())
    })
}

fn parse_lines(raw: &str) -> Vec<JsonValue> {
    raw.split('\n')
        .filter(|l| !l.is_empty())
        .filter_map(|l| l.parse::<JsonValue>().ok())
        .collect()
}

/// Read AND clear a recipient's inbox in one locked step.
pub fn drain(recipient_id: &str) -> Result<Vec<JsonValue>, String> {
    with_lock(|| {
        let path = mailbox_path(recipient_id);
        let raw = match fs::read_to_string(&path) {
            Ok(r) => r,
            Err(_) => return Ok(Vec::new()),
        };
        let msgs = parse_lines(&raw);
        let _ = fs::remove_file(&path);
        Ok(msgs)
    })
}

pub fn peek(recipient_id: &str) -> Vec<JsonValue> {
    fs::read_to_string(mailbox_path(recipient_id))
        .map(|raw| parse_lines(&raw))
        .unwrap_or_default()
}

/// The watch poll loop only needs presence, not parsed messages. Avoid reading
/// and allocating the entire mailbox every tick; the eventual drain remains
/// the authority for parsing and clearing it.
pub fn mailbox_has_content(recipient_id: &str) -> bool {
    fs::metadata(mailbox_path(recipient_id))
        .map(|metadata| metadata.len() > 0)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_maps_traversal_to_dashes() {
        assert_eq!(
            sanitize("../../etc/passwd"),
            ".._.._etc_passwd".replace('_', "-")
        );
        assert_eq!(sanitize("ok-1.2_x"), "ok-1.2_x");
        assert_eq!(sanitize("a/b\\c:d"), "a-b-c-d");
    }

    #[test]
    fn encode_dir_collapses_non_alnum() {
        let e = encode_dir("/tmp/some dir/x");
        assert!(e.starts_with("-tmp-some-dir"));
        assert!(e.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    }

    #[test]
    fn civil_from_days_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }

    #[test]
    fn iso_now_shape() {
        let s = iso_now();
        assert_eq!(s.len(), 24);
        assert!(s.ends_with('Z'));
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[10..11], "T");
        assert_eq!(&s[19..20], ".");
    }

    #[test]
    fn uuid_v4_shape() {
        let u = uuid_v4();
        assert_eq!(u.len(), 36);
        assert_eq!(&u[14..15], "4");
        assert!(matches!(&u[19..20], "8" | "9" | "a" | "b"));
    }
}
