// store.rs — shared on-disk state for the session-relay bus (port of lib/store.mjs).
// Holds three things, all under one fixed home so every component agrees:
//   registry.json      id -> { id, dir, name, tool, lastSeen } + a name -> id index
//   lifecycle-v1.json durable lifecycle authority, isolated from legacy writers
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

use rustix::fd::OwnedFd;
use rustix::fs::{
    AtFlags, Dir, FileType, FlockOperation, Mode, OFlags, flock, fstat, open, openat, renameat,
    statat, unlinkat,
};
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

const LIFECYCLE_AUTHORITY_FILE: &str = "lifecycle-v1.json";
const LIFECYCLE_AUTHORITY_SCHEMA: &str = "1";
pub(crate) fn mailbox_path(id: &str) -> PathBuf {
    home_dir()
        .join("mailbox")
        .join(format!("{}.jsonl", sanitize(id)))
}
fn marker_path(dir: &str) -> PathBuf {
    home_dir().join("markers").join(encode_dir(dir))
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

fn ensure_dirs_at(root: &Path) -> Result<(), String> {
    for d in ["mailbox", "markers", "watchers", "locks"] {
        let path = root.join(d);
        fs::create_dir_all(&path).map_err(|e| format!("mkdir {d}: {e}"))?;
        if matches!(d, "watchers" | "locks") {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
                .map_err(|e| format!("chmod {}: {e}", path.display()))?;
        }
    }
    Ok(())
}

fn ensure_dirs() -> Result<(), String> {
    ensure_dirs_at(&home_dir())
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

/// True only while a kernel-held watcher lock advertises the requested mode.
/// A holder dying between the two probes can cause one conservative false
/// positive (mail stays queued for the next hook), never a wrong drain.
pub fn live_watcher_mode(id: &str, mode: &str) -> bool {
    watcher_status(id) == LockStatus::Live
        && read_lock_metadata(&watcher_lock_path(id)).is_some_and(|metadata| metadata.mode == mode)
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

fn atomic_write_private(file: &Path, text: &str) -> Result<(), String> {
    let tmp = PathBuf::from(format!(
        "{}.{}.{}.tmp",
        file.display(),
        std::process::id(),
        random_hex(4)
    ));
    let result = (|| {
        let mut output = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|error| format!("create {}: {error}", tmp.display()))?;
        output
            .write_all(text.as_bytes())
            .map_err(|error| format!("write {}: {error}", tmp.display()))?;
        output
            .sync_all()
            .map_err(|error| format!("sync {}: {error}", tmp.display()))?;
        fs::rename(&tmp, file).map_err(|error| format!("rename to {}: {error}", file.display()))?;
        if let Some(parent) = file.parent() {
            fs::File::open(parent)
                .and_then(|directory| directory.sync_all())
                .map_err(|error| format!("sync {}: {error}", parent.display()))?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Run `f` holding an exclusive kernel flock on <home>/.lock.
/// Fail-fast contract preserved from the Node store: give up after 3s of
/// live contention. Crashed holders need no reclaim — the kernel releases.
pub fn with_lock<T>(f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    with_lock_at(&home_dir(), f)
}

pub(crate) fn with_lock_at<T>(
    root: &Path,
    f: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    ensure_dirs_at(root)?;
    let lock = root.join(".lock");
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
    pub server: Option<String>,
    pub spawned_via: Option<String>,
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
        m.insert(
            "server".into(),
            self.server
                .clone()
                .map(JsonValue::from)
                .unwrap_or(JsonValue::from(())),
        );
        m.insert(
            "spawned_via".into(),
            self.spawned_via
                .clone()
                .map(JsonValue::from)
                .unwrap_or(JsonValue::from(())),
        );
        JsonValue::from(m)
    }

    pub(crate) fn from_json(v: &JsonValue) -> Option<Entry> {
        let obj: &HashMap<String, JsonValue> = v.get()?;
        let s = |k: &str| -> Option<String> { obj.get(k)?.get::<String>().cloned() };
        Some(Entry {
            id: s("id")?,
            dir: s("dir"),
            name: s("name"),
            tool: s("tool").unwrap_or_else(|| "claude".to_string()),
            last_seen: s("lastSeen").unwrap_or_default(),
            server: s("server"),
            spawned_via: s("spawned_via"),
        })
    }
}

#[derive(Clone, Default)]
pub(crate) struct Registry {
    pub(crate) agents: HashMap<String, JsonValue>,
    pub(crate) names: HashMap<String, JsonValue>,
    pub(crate) extra: HashMap<String, JsonValue>,
}

// Any read/parse failure yields an empty registry — mirrors readJSON(file, fallback).
fn read_registry() -> Registry {
    read_registry_at(&home_dir())
}

pub(crate) fn read_registry_at(root: &Path) -> Registry {
    parse_registry(
        fs::read_to_string(root.join("registry.json"))
            .ok()
            .as_deref(),
    )
}

fn parse_registry(raw: Option<&str>) -> Registry {
    let mut registry = Registry::default();
    if let Some(raw) = raw {
        if let Ok(v) = raw.parse::<JsonValue>() {
            if let Some(obj) = v.get::<HashMap<String, JsonValue>>() {
                registry.extra = obj.clone();
                if let Some(a) = obj
                    .get("agents")
                    .and_then(|x| x.get::<HashMap<String, JsonValue>>())
                {
                    registry.agents = a.clone();
                }
                if let Some(n) = obj
                    .get("names")
                    .and_then(|x| x.get::<HashMap<String, JsonValue>>())
                {
                    registry.names = n.clone();
                }
                registry.extra.remove("agents");
                registry.extra.remove("names");
            }
        }
    }
    registry
}

fn write_registry(registry: Registry) -> Result<(), String> {
    let text = format_registry(registry)?;
    atomic_write(&registry_path(), &text)
}

pub(crate) fn write_registry_at(root: &Path, registry: Registry) -> Result<(), String> {
    let text = format_registry(registry)?;
    atomic_write(&root.join("registry.json"), &text)
}

pub(crate) fn read_lifecycle_authority_at(
    root: &Path,
) -> Result<Option<HashMap<String, JsonValue>>, String> {
    let path = root.join(LIFECYCLE_AUTHORITY_FILE);
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "read lifecycle authority {}: {error}",
                path.display()
            ));
        }
    };
    let value = raw
        .parse::<JsonValue>()
        .map_err(|error| format!("malformed lifecycle authority: {error}"))?;
    let object = value
        .get::<HashMap<String, JsonValue>>()
        .ok_or_else(|| "malformed lifecycle authority: root is not an object".to_string())?;
    if object.len() != 2
        || object
            .get("schema_version")
            .and_then(JsonValue::get::<String>)
            .map(String::as_str)
            != Some(LIFECYCLE_AUTHORITY_SCHEMA)
    {
        return Err("malformed lifecycle authority: unsupported or inexact schema".to_string());
    }
    let state = object
        .get("state")
        .and_then(JsonValue::get::<HashMap<String, JsonValue>>)
        .ok_or_else(|| "malformed lifecycle authority: state is not an object".to_string())?;
    Ok(Some(state.clone()))
}

pub(crate) fn write_lifecycle_authority_at(
    root: &Path,
    state: HashMap<String, JsonValue>,
) -> Result<(), String> {
    let mut object = HashMap::new();
    object.insert(
        "schema_version".to_string(),
        JsonValue::from(LIFECYCLE_AUTHORITY_SCHEMA.to_string()),
    );
    object.insert("state".to_string(), JsonValue::from(state));
    let text = JsonValue::from(object)
        .format()
        .map_err(|error| format!("lifecycle authority serialize: {error}"))?;
    atomic_write_private(&root.join(LIFECYCLE_AUTHORITY_FILE), &text)
}

fn format_registry(mut registry: Registry) -> Result<String, String> {
    let mut root = std::mem::take(&mut registry.extra);
    root.insert("agents".into(), JsonValue::from(registry.agents));
    root.insert("names".into(), JsonValue::from(registry.names));
    JsonValue::from(root)
        .format() // pretty, 2-space — same shape JSON.stringify(reg, null, 2) wrote
        .map_err(|e| format!("registry serialize: {e}"))
}

#[derive(Default)]
struct GcCandidate {
    id: Option<String>,
    registry_key: Option<String>,
    last_seen: Option<String>,
    surfaces: Vec<GcSurface>,
}

struct GcSurfaceDir {
    name: &'static str,
    fd: OwnedFd,
}

#[derive(Clone)]
struct GcSurface {
    directory: &'static str,
    name: String,
    dev: i128,
    ino: i128,
    size: i128,
    mtime: i128,
    mtime_nsec: i128,
}

#[derive(Default)]
struct GcInventory {
    surfaces: Vec<(Option<String>, String, GcSurface)>,
    fresh_unknown_markers: HashMap<String, GcSurface>,
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
    surface: GcSurface,
) {
    let key = id
        .as_ref()
        .map(|id| format!("session:{id}"))
        .unwrap_or(orphan_key);
    let candidate = candidates.entry(key).or_default();
    if candidate.id.is_none() {
        candidate.id = id;
    }
    candidate.surfaces.push(surface);
}

fn open_gc_surface_dirs(root_fd: &OwnedFd) -> Result<Vec<GcSurfaceDir>, String> {
    let mut dirs = Vec::new();
    for name in ["mailbox", "markers", "watchers", "locks", "spawn-logs"] {
        match openat(
            root_fd,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => dirs.push(GcSurfaceDir { name, fd }),
            Err(e) if e == rustix::io::Errno::NOENT => {}
            Err(e) => {
                return Err(format!(
                    "refusing GC: relay surface directory {name} is not a no-follow directory: {e}"
                ));
            }
        }
    }
    Ok(dirs)
}

fn gc_surface_dir<'a>(dirs: &'a [GcSurfaceDir], name: &str) -> Option<&'a GcSurfaceDir> {
    dirs.iter().find(|dir| dir.name == name)
}

fn gc_surface_id(directory: &str, name: &str) -> Option<String> {
    let id = match directory {
        "mailbox" => name.strip_suffix(".jsonl")?,
        "watchers" => name
            .strip_suffix(".lock")
            .or_else(|| name.strip_suffix(".progress"))?,
        "locks" => name.strip_prefix("resume-")?.strip_suffix(".lock")?,
        "spawn-logs" => name.strip_suffix(".stderr")?,
        _ => return None,
    };
    is_uuid(id).then(|| id.to_string())
}

fn open_regular_at(
    dir: &OwnedFd,
    name: &str,
) -> Result<Option<(fs::File, rustix::fs::Stat)>, String> {
    let fd = match openat(
        dir,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(e) if e == rustix::io::Errno::NOENT || e == rustix::io::Errno::LOOP => return Ok(None),
        Err(e) => return Err(format!("open GC surface {name}: {e}")),
    };
    let stat = fstat(&fd).map_err(|e| format!("stat GC surface {name}: {e}"))?;
    if !FileType::from_raw_mode(stat.st_mode).is_file() {
        return Ok(None);
    }
    Ok(Some((fs::File::from(fd), stat)))
}

fn stat_regular_surface_at(dir: &OwnedFd, name: &str) -> Result<Option<rustix::fs::Stat>, String> {
    let stat = match statat(dir, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(e) if e == rustix::io::Errno::NOENT => return Ok(None),
        Err(e) => return Err(format!("stat GC surface {name}: {e}")),
    };
    Ok(FileType::from_raw_mode(stat.st_mode)
        .is_file()
        .then_some(stat))
}

fn gc_surface(directory: &'static str, name: String, stat: &rustix::fs::Stat) -> GcSurface {
    GcSurface {
        directory,
        name,
        dev: i128::from(stat.st_dev),
        ino: i128::from(stat.st_ino),
        size: i128::from(stat.st_size),
        mtime: i128::from(stat.st_mtime),
        mtime_nsec: i128::from(stat.st_mtime_nsec),
    }
}

fn known_gc_surfaces(dirs: &[GcSurfaceDir], cutoff: SystemTime) -> Result<GcInventory, String> {
    let mut inventory = GcInventory::default();
    for surface_dir in dirs {
        let directory = surface_dir.name;
        let entries = Dir::read_from(&surface_dir.fd)
            .map_err(|e| format!("read GC surface {directory}: {e}"))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("read GC surface {directory}: {e}"))?;
            let Some(name) = entry.file_name().to_str().ok().map(str::to_string) else {
                continue;
            };
            if directory == "markers" {
                let Some(stat) = stat_regular_surface_at(&surface_dir.fd, &name)? else {
                    continue;
                };
                let surface = gc_surface(directory, name.clone(), &stat);
                let mut raw = String::new();
                let marker_id = match open_regular_at(&surface_dir.fd, &name) {
                    Ok(Some((mut file, _))) => file
                        .read_to_string(&mut raw)
                        .ok()
                        .and_then(|_| is_uuid(raw.trim()).then(|| raw.trim().to_string())),
                    _ => None,
                };
                if let Some(id) = marker_id {
                    inventory.surfaces.push((
                        Some(id),
                        format!("orphan:{directory}:{name}"),
                        surface,
                    ));
                } else if !surface_is_old(&surface, cutoff) {
                    // A fresh marker whose owner cannot be read must still
                    // participate in all-surfaces-old via its encoded cwd.
                    inventory.fresh_unknown_markers.insert(name, surface);
                }
                continue;
            }
            // Foreign names are never opened or statted. Classification must
            // precede all filesystem inspection to avoid GC denial.
            let Some(id) = gc_surface_id(directory, &name) else {
                continue;
            };
            let Some(stat) = stat_regular_surface_at(&surface_dir.fd, &name)? else {
                continue;
            };
            inventory.surfaces.push((
                Some(id),
                format!("orphan:{directory}:{name}"),
                gc_surface(directory, name, &stat),
            ));
        }
    }
    Ok(inventory)
}

fn surface_is_old(surface: &GcSurface, cutoff: SystemTime) -> bool {
    let cutoff = cutoff.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
    let cutoff_secs = i128::from(cutoff.as_secs());
    surface.mtime < cutoff_secs
        || (surface.mtime == cutoff_secs && surface.mtime_nsec <= i128::from(cutoff.subsec_nanos()))
}

fn surface_matches_snapshot(stat: &rustix::fs::Stat, surface: &GcSurface) -> bool {
    FileType::from_raw_mode(stat.st_mode).is_file()
        && i128::from(stat.st_dev) == surface.dev
        && i128::from(stat.st_ino) == surface.ino
        && i128::from(stat.st_size) == surface.size
        && i128::from(stat.st_mtime) == surface.mtime
        && i128::from(stat.st_mtime_nsec) == surface.mtime_nsec
}

fn candidate_surfaces_still_eligible(
    dirs: &[GcSurfaceDir],
    candidate: &GcCandidate,
    cutoff: SystemTime,
) -> Result<bool, String> {
    for surface in &candidate.surfaces {
        let Some(dir) = gc_surface_dir(dirs, surface.directory) else {
            return Ok(false);
        };
        let Some(stat) = stat_regular_surface_at(&dir.fd, &surface.name)? else {
            return Ok(false);
        };
        if !surface_matches_snapshot(&stat, surface) || !surface_is_old(surface, cutoff) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn lock_status_at(dir: Option<&GcSurfaceDir>, name: &str) -> LockStatus {
    let Some(dir) = dir else {
        return LockStatus::Never;
    };
    let fd = match openat(
        &dir.fd,
        name,
        OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(e) if e == rustix::io::Errno::NOENT => return LockStatus::Never,
        Err(_) => return LockStatus::Unknown,
    };
    match flock(&fd, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {
            let _ = flock(&fd, FlockOperation::Unlock);
            LockStatus::Dead
        }
        Err(e) if e == rustix::io::Errno::AGAIN => LockStatus::Live,
        Err(_) => LockStatus::Unknown,
    }
}

fn acquire_gc_spawn_log_guard(dir: Option<&GcSurfaceDir>, name: &str) -> Option<OwnedFd> {
    let dir = dir?;
    let fd = openat(
        &dir.fd,
        name,
        OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .ok()?;
    let stat = fstat(&fd).ok()?;
    if !FileType::from_raw_mode(stat.st_mode).is_file() {
        return None;
    }
    flock(&fd, FlockOperation::NonBlockingLockExclusive)
        .ok()
        .map(|()| fd)
}

fn read_text_at(root_fd: &OwnedFd, name: &str) -> Result<Option<String>, String> {
    let fd = match openat(
        root_fd,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(e) if e == rustix::io::Errno::NOENT => return Ok(None),
        Err(e) => return Err(format!("open relay store file {name}: {e}")),
    };
    let stat = fstat(&fd).map_err(|e| format!("stat relay store file {name}: {e}"))?;
    if !FileType::from_raw_mode(stat.st_mode).is_file() {
        return Err(format!(
            "refusing GC: relay store file {name} is not a regular file"
        ));
    }
    let mut raw = String::new();
    fs::File::from(fd)
        .read_to_string(&mut raw)
        .map_err(|e| format!("read relay store file {name}: {e}"))?;
    Ok(Some(raw))
}

fn stat_regular_at(root_fd: &OwnedFd, name: &str) -> Result<Option<rustix::fs::Stat>, String> {
    let stat = match statat(root_fd, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(e) if e == rustix::io::Errno::NOENT => return Ok(None),
        Err(e) => return Err(format!("stat relay store file {name}: {e}")),
    };
    if !FileType::from_raw_mode(stat.st_mode).is_file() {
        return Err(format!(
            "refusing GC: relay store file {name} is not a regular file"
        ));
    }
    Ok(Some(stat))
}

fn atomic_write_at(root_fd: &OwnedFd, name: &str, text: &str) -> Result<(), String> {
    let tmp = format!(".{name}.{}.{}.tmp", std::process::id(), random_hex(4));
    let fd = openat(
        root_fd,
        &tmp,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )
    .map_err(|e| format!("create relay store temp {tmp}: {e}"))?;
    let write = fs::File::from(fd)
        .write_all(text.as_bytes())
        .map_err(|e| format!("write relay store temp {tmp}: {e}"));
    if let Err(e) = write {
        let _ = unlinkat(root_fd, &tmp, AtFlags::empty());
        return Err(e);
    }
    if let Err(e) = renameat(root_fd, &tmp, root_fd, name) {
        let _ = unlinkat(root_fd, &tmp, AtFlags::empty());
        return Err(format!("rename relay store temp to {name}: {e}"));
    }
    Ok(())
}

fn with_gc_lock<T>(root_fd: &OwnedFd, f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    let lock = openat(
        root_fd,
        ".lock",
        OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )
    .map_err(|e| format!("open GC lock: {e}"))?;
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match flock(&lock, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => break,
            Err(e) if e == rustix::io::Errno::AGAIN || e == rustix::io::Errno::INTR => {
                if Instant::now() > deadline {
                    return Err("session-relay: lock busy (held > 3s)".to_string());
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => return Err(format!("flock GC lock: {e}")),
        }
    }
    let out = f();
    let _ = flock(&lock, FlockOperation::Unlock);
    out
}

fn remove_gc_surface(dirs: &[GcSurfaceDir], surface: &GcSurface) -> Result<bool, String> {
    let Some(dir) = gc_surface_dir(dirs, surface.directory) else {
        return Ok(false);
    };
    let stat = match statat(&dir.fd, &surface.name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(e) if e == rustix::io::Errno::NOENT => return Ok(false),
        Err(e) => {
            return Err(format!(
                "stat GC surface {}/{}: {e}",
                surface.directory, surface.name
            ));
        }
    };
    if !surface_matches_snapshot(&stat, surface) {
        return Err(format!(
            "refusing GC: relay surface changed during sweep: {}/{}",
            surface.directory, surface.name
        ));
    }
    unlinkat(&dir.fd, &surface.name, AtFlags::empty()).map_err(|e| {
        format!(
            "remove GC surface {}/{}: {e}",
            surface.directory, surface.name
        )
    })?;
    Ok(true)
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
    let root_fd = open(
        &resolved_root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| format!("refusing GC: open pinned relay store root: {e}"))?;
    // Validate and pin every present surface before the GC lock can create its
    // file. A symlinked surface refuses the whole sweep without chmod/create.
    let surface_dirs = open_gc_surface_dirs(&root_fd)?;
    let throttled = with_gc_lock(&root_fd, || {
        let Some(stat) = stat_regular_at(&root_fd, "gc-stamp")? else {
            return Ok(false);
        };
        let modified = UNIX_EPOCH
            .checked_add(Duration::new(
                stat.st_mtime.max(0) as u64,
                stat.st_mtime_nsec.clamp(0, 999_999_999) as u32,
            ))
            .unwrap_or(UNIX_EPOCH);
        Ok(now.duration_since(modified).unwrap_or(Duration::ZERO) < GC_INTERVAL)
    })?;
    if throttled {
        return Ok(0);
    }
    let lifecycle_removed = crate::lifecycle::LifecycleStore::default()
        .gc_unmanaged_excluding(now, crate::lifecycle::GcControl::RunToCompletion, self_id)?
        .removed_candidates;
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

    let legacy_removed = with_gc_lock(&root_fd, || {
        let Some((locked_root, locked_resolved_root)) = safe_existing_root()? else {
            return Ok(0);
        };
        if locked_root != root || locked_resolved_root != resolved_root {
            return Err("refusing GC: relay store root changed during sweep".to_string());
        }
        let current_root_fd = open(
            &locked_resolved_root,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| format!("refusing GC: re-open relay store root: {e}"))?;
        let pinned = fstat(&root_fd).map_err(|e| format!("stat pinned relay store root: {e}"))?;
        let current = fstat(&current_root_fd).map_err(|e| format!("stat relay store root: {e}"))?;
        if i128::from(pinned.st_dev) != i128::from(current.st_dev)
            || i128::from(pinned.st_ino) != i128::from(current.st_ino)
        {
            return Err("refusing GC: relay store root changed during sweep".to_string());
        }
        if let Some(stat) = stat_regular_at(&root_fd, "gc-stamp")? {
            let modified = UNIX_EPOCH
                .checked_add(Duration::new(
                    stat.st_mtime.max(0) as u64,
                    stat.st_mtime_nsec.clamp(0, 999_999_999) as u32,
                ))
                .unwrap_or(UNIX_EPOCH);
            if now.duration_since(modified).unwrap_or(Duration::ZERO) < GC_INTERVAL {
                return Ok(0);
            }
        }

        let registry_raw = read_text_at(&root_fd, "registry.json")?;
        let mut registry = parse_registry(registry_raw.as_deref());
        let mut protection_registry = registry.clone();
        if let Some(authority) = read_lifecycle_authority_at(&locked_resolved_root)? {
            protection_registry.extra.extend(authority);
        }
        let inventory = known_gc_surfaces(&surface_dirs, cutoff)?;
        let mut candidates: HashMap<String, GcCandidate> = HashMap::new();
        for (registry_key, value) in &registry.agents {
            let Some(entry) = Entry::from_json(value) else {
                continue; // malformed registry state is preserved, never guessed old
            };
            if !is_uuid(registry_key) || entry.id != *registry_key {
                continue;
            }
            let candidate = candidates
                .entry(format!("session:{registry_key}"))
                .or_default();
            candidate.id = Some(registry_key.clone());
            candidate.registry_key = Some(registry_key.clone());
            if let Some(dir) = entry.dir.as_deref() {
                if let Some(marker) = inventory.fresh_unknown_markers.get(&encode_dir(dir)) {
                    candidate.surfaces.push(marker.clone());
                }
            }
            candidate.last_seen = Some(entry.last_seen);
        }
        for (id, orphan_key, surface) in inventory.surfaces {
            add_gc_surface(&mut candidates, id, orphan_key, surface);
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
                .surfaces
                .iter()
                .all(|surface| surface_is_old(surface, cutoff))
            {
                continue;
            }
            if let Some(id) = candidate.id.as_deref() {
                if crate::lifecycle::registry_protects_session(&protection_registry, id)? {
                    continue;
                }
                let watcher = format!("{id}.lock");
                let resume = format!("resume-{id}.lock");
                if matches!(
                    lock_status_at(gc_surface_dir(&surface_dirs, "watchers"), &watcher),
                    LockStatus::Live | LockStatus::Unknown
                ) || matches!(
                    lock_status_at(gc_surface_dir(&surface_dirs, "locks"), &resume),
                    LockStatus::Live | LockStatus::Unknown
                ) {
                    continue;
                }
            }
            eligible.push(key.clone());
        }

        let mut removed_files = 0;
        let mut removed_registry_ids = HashSet::new();
        'candidate: for key in &eligible {
            let candidate = &candidates[key];
            let mut spawn_log_guards = Vec::new();
            for surface in candidate
                .surfaces
                .iter()
                .filter(|surface| surface.directory == "spawn-logs")
            {
                let Some(guard) = acquire_gc_spawn_log_guard(
                    gc_surface_dir(&surface_dirs, "spawn-logs"),
                    &surface.name,
                ) else {
                    continue 'candidate;
                };
                spawn_log_guards.push(guard);
            }
            // All-or-nothing preflight: a surface that became fresh or was
            // replaced since enumeration preserves the whole candidate.
            if !candidate_surfaces_still_eligible(&surface_dirs, candidate, cutoff)? {
                continue;
            }
            for surface in &candidate.surfaces {
                if remove_gc_surface(&surface_dirs, surface)? {
                    removed_files += 1;
                }
            }
            if let Some(registry_key) = &candidate.registry_key {
                removed_registry_ids.insert(registry_key.clone());
            }
            drop(spawn_log_guards);
        }

        // Registry is last: an interrupted sweep leaves visible entries that
        // a later pass can retry, never invisible orphan files.
        if !removed_registry_ids.is_empty() {
            for id in &removed_registry_ids {
                registry.agents.remove(id);
            }
            registry.names.retain(|_, value| {
                value
                    .get::<String>()
                    .is_none_or(|id| !removed_registry_ids.contains(id))
            });
            let registry_text = format_registry(registry)?;
            atomic_write_at(&root_fd, "registry.json", &registry_text)?;
        }
        atomic_write_at(&root_fd, "gc-stamp", &format!("{}\n", iso_now()))?;
        Ok(removed_files + removed_registry_ids.len())
    })?;
    Ok(lifecycle_removed + legacy_removed)
}

/// Upsert a session. Missing fields are preserved from any prior entry, so the
/// hook (id + dir, no name) and a later register(name) compose cleanly.
pub fn register(
    id: &str,
    dir: Option<&str>,
    name: Option<&str>,
    tool: Option<&str>,
    server: Option<&str>,
) -> Result<Entry, String> {
    register_with_origin(id, dir, name, tool, server, None)
}

pub fn register_with_origin(
    id: &str,
    dir: Option<&str>,
    name: Option<&str>,
    tool: Option<&str>,
    server: Option<&str>,
    spawned_via: Option<&str>,
) -> Result<Entry, String> {
    if id.is_empty() {
        return Err("register requires an id".to_string());
    }
    with_lock(|| {
        let mut registry = read_registry();
        let prev = registry.agents.get(id).and_then(Entry::from_json);
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
            server: server
                .map(str::to_string)
                .or_else(|| prev.as_ref().and_then(|p| p.server.clone())),
            spawned_via: spawned_via
                .map(str::to_string)
                .or_else(|| prev.as_ref().and_then(|p| p.spawned_via.clone())),
        };
        registry.agents.insert(id.to_string(), entry.to_json());
        if let Some(n) = &entry.name {
            // drop a renamed alias: any other name bound to this id
            registry
                .names
                .retain(|k, v| !(v.get::<String>().map(|s| s == id).unwrap_or(false) && k != n));
            registry
                .names
                .insert(n.clone(), JsonValue::from(id.to_string()));
        }
        write_registry(registry)?;
        Ok(entry)
    })
}

pub fn roster() -> Vec<Entry> {
    let registry = read_registry();
    let mut out: Vec<Entry> = registry
        .agents
        .values()
        .filter_map(Entry::from_json)
        .collect();
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
    let registry = read_registry();
    if let Some(e) = registry.agents.get(name_or_id).and_then(Entry::from_json) {
        return Some(e);
    }
    let id = registry.names.get(name_or_id)?.get::<String>()?.clone();
    registry.agents.get(&id).and_then(Entry::from_json)
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

/// Read AND clear the guard-bound inbox in one locked step. The recipient is
/// derived from the sealed lifecycle capability; callers cannot supply one.
pub struct DrainReceipt {
    messages: Vec<JsonValue>,
    raw: String,
    path: PathBuf,
    root: PathBuf,
    _sealed: (),
}

impl DrainReceipt {
    pub fn messages(&self) -> &[JsonValue] {
        &self.messages
    }

    pub fn into_messages(self) -> Vec<JsonValue> {
        self.messages
    }

    pub fn commit(self) {}

    pub fn rollback(self) -> Result<(), String> {
        with_lock_at(&self.root, || {
            let current = fs::read_to_string(&self.path).unwrap_or_default();
            let mut restored = self.raw;
            restored.push_str(&current);
            if restored.is_empty() {
                return Ok(());
            }
            fs::write(&self.path, restored).map_err(|error| {
                format!("restore drained mailbox {}: {error}", self.path.display())
            })
        })
    }
}

pub fn drain_with_guard(
    guard: &mut crate::lifecycle::ReentryGuard,
) -> Result<DrainReceipt, String> {
    let kind = guard.allowed();
    if !matches!(
        kind,
        crate::lifecycle::OperationKind::SessionStartDrain
            | crate::lifecycle::OperationKind::UserPromptDrain
            | crate::lifecycle::OperationKind::CliInboxDrain
            | crate::lifecycle::OperationKind::McpInboxDrain
            | crate::lifecycle::OperationKind::ChannelDeliver
            | crate::lifecycle::OperationKind::WatchInject
            | crate::lifecycle::OperationKind::WatchAutoTurn
            | crate::lifecycle::OperationKind::WakeAppServer
    ) {
        return Err(format!("{} cannot drain a mailbox", kind.as_str()));
    }
    guard.with_authorized(kind, |target| {
        let path = target
            .root
            .join("mailbox")
            .join(format!("{}.jsonl", sanitize(&target.runtime_session_id)));
        let raw = fs::read_to_string(&path).unwrap_or_default();
        let msgs = parse_lines(&raw);
        let _ = fs::remove_file(&path);
        Ok(DrainReceipt {
            messages: msgs,
            raw,
            path,
            root: target.root.clone(),
            _sealed: (),
        })
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

    #[test]
    fn entry_roundtrips_server_and_origin_while_legacy_entries_default_to_none() {
        let entry = Entry {
            id: "11111111-1111-4111-8111-111111111111".into(),
            dir: Some("/tmp/project".into()),
            name: Some("worker".into()),
            tool: "codex".into(),
            last_seen: "2026-07-10T00:00:00.000Z".into(),
            server: Some("/tmp/app.sock".into()),
            spawned_via: Some("app-server".into()),
        };
        assert_eq!(Entry::from_json(&entry.to_json()), Some(entry.clone()));

        let mut legacy = entry
            .to_json()
            .get::<HashMap<String, JsonValue>>()
            .cloned()
            .expect("entry object");
        legacy.remove("server");
        legacy.remove("spawned_via");
        let legacy_entry = Entry::from_json(&JsonValue::from(legacy)).unwrap();
        assert_eq!(legacy_entry.server, None);
        assert_eq!(legacy_entry.spawned_via, None);
    }

    #[test]
    fn changed_surface_invalidates_the_whole_candidate_before_deletion() {
        let root = std::env::temp_dir().join(format!("relay-gc-freshness-{}", uuid_v4()));
        let spawn_logs = root.join("spawn-logs");
        fs::create_dir_all(&spawn_logs).expect("create GC freshness fixture");
        let id = "50505050-5050-4050-8050-505050505050";
        let path = spawn_logs.join(format!("{id}.stderr"));
        fs::write(&path, b"old").expect("write old surface");

        let root_fd = open(
            &root,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .expect("open fixture root");
        let dirs = open_gc_surface_dirs(&root_fd).expect("open fixture surfaces");
        let cutoff = SystemTime::now() + Duration::from_secs(60);
        let (_, _, surface) = known_gc_surfaces(&dirs, cutoff)
            .expect("enumerate fixture")
            .surfaces
            .into_iter()
            .find(|(surface_id, _, _)| surface_id.as_deref() == Some(id))
            .expect("find spawn surface");
        let candidate = GcCandidate {
            id: Some(id.to_string()),
            surfaces: vec![surface],
            ..GcCandidate::default()
        };
        assert!(candidate_surfaces_still_eligible(&dirs, &candidate, cutoff).unwrap());

        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .and_then(|mut file| file.write_all(b"-fresh"))
            .expect("refresh surface after enumeration");

        assert!(!candidate_surfaces_still_eligible(&dirs, &candidate, cutoff).unwrap());
        assert!(path.exists(), "changed candidate remains intact");
        drop(dirs);
        drop(root_fd);
        fs::remove_dir_all(root).expect("remove GC freshness fixture");
    }
}
