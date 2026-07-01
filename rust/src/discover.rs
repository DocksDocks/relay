// discover.rs — find agent sessions running RIGHT NOW by scanning the raw
// on-disk session stores (port of lib/discover.mjs), so the bus can
// auto-resolve "my other session" with NO prior bus registration.
//   Claude: <root>/<encoded-cwd>/<session-id>.jsonl — the id IS the filename;
//           the dir name is a LOSSY cwd encoding, so the real cwd is read from
//           file content (first line carrying a "cwd" field).
//   Codex:  <root>/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl — first line is a
//           session_meta event whose payload has id + cwd.
// Liveness = mtime recency; files are stat-filtered by the window BEFORE any
// content is read. Non-UUID ids are dropped (planted/garbage, and it keeps
// them off the doorbell argv). Read-only — never mutates a store.

use crate::store;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use tinyjson::JsonValue;

const READ_CAP: usize = 65536; // bytes scanned per file to find cwd / the meta line

fn env_nonempty(var: &str) -> Option<String> {
    std::env::var(var).ok().filter(|v| !v.is_empty())
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".to_string()))
}

fn claude_root() -> PathBuf {
    if let Some(v) = env_nonempty("RELAY_CLAUDE_PROJECTS") {
        return PathBuf::from(v);
    }
    let base = env_nonempty("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".claude"));
    base.join("projects")
}

fn codex_root() -> PathBuf {
    if let Some(v) = env_nonempty("RELAY_CODEX_SESSIONS") {
        return PathBuf::from(v);
    }
    let base = env_nonempty("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".codex"));
    base.join("sessions")
}

fn mtime_ms(file: &Path) -> i64 {
    fs::metadata(file)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// Read the first READ_CAP bytes as whole lines (drops a trailing partial line,
// but never empties a single long line). Session transcripts can be megabytes.
fn head_lines(file: &Path) -> Vec<String> {
    let Ok(mut f) = fs::File::open(file) else {
        return Vec::new();
    };
    let mut buf = vec![0u8; READ_CAP];
    let mut n = 0;
    while n < READ_CAP {
        match f.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(_) => return Vec::new(),
        }
    }
    let text = String::from_utf8_lossy(&buf[..n]);
    let mut lines: Vec<String> = text.split('\n').map(str::to_string).collect();
    if n == READ_CAP && lines.len() > 1 {
        lines.pop(); // last line may be truncated
    }
    lines
}

fn as_obj(v: &JsonValue) -> Option<&HashMap<String, JsonValue>> {
    v.get::<HashMap<String, JsonValue>>()
}
fn str_field(obj: &HashMap<String, JsonValue>, key: &str) -> Option<String> {
    obj.get(key)?
        .get::<String>()
        .filter(|s| !s.is_empty())
        .cloned()
}

// Claude: the cwd lives in the file content, not the (lossy) dir name.
fn claude_cwd(file: &Path) -> Option<String> {
    for l in head_lines(file) {
        if l.trim().is_empty() || !l.contains("\"cwd\"") {
            continue;
        }
        if let Ok(j) = l.parse::<JsonValue>() {
            if let Some(cwd) = as_obj(&j).and_then(|o| str_field(o, "cwd")) {
                return Some(cwd);
            }
        }
    }
    None
}

// Codex: the first non-blank line is the session_meta event (payload.id + payload.cwd).
fn codex_meta(file: &Path) -> Option<(Option<String>, Option<String>)> {
    for l in head_lines(file) {
        if l.trim().is_empty() {
            continue;
        }
        let j = l.parse::<JsonValue>().ok()?; // unparseable first line → give up (Node parity)
        let root = as_obj(&j)?;
        let payload = root.get("payload").and_then(as_obj).unwrap_or(root);
        let id = str_field(payload, "id").or_else(|| str_field(payload, "session_id"));
        let cwd = str_field(payload, "cwd");
        return Some((id, cwd));
    }
    None
}

struct Candidate {
    tool: &'static str,
    id: Option<String>,
    file: PathBuf,
    last_activity_ms: i64,
}

// Cheap enumeration: candidates + mtime, WITHOUT reading content.
fn list_claude_files() -> Vec<Candidate> {
    let mut out = Vec::new();
    let Ok(projects) = fs::read_dir(claude_root()) else {
        return out;
    };
    for proj in projects.flatten() {
        if !proj.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let Ok(ents) = fs::read_dir(proj.path()) else {
            continue;
        };
        for e in ents.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !e.file_type().map(|t| t.is_file()).unwrap_or(false) || !name.ends_with(".jsonl") {
                continue;
            }
            let file = e.path();
            out.push(Candidate {
                tool: "claude",
                id: Some(name[..name.len() - ".jsonl".len()].to_string()),
                last_activity_ms: mtime_ms(&file),
                file,
            });
        }
    }
    out
}

fn list_codex_files() -> Vec<Candidate> {
    let mut out = Vec::new();
    fn walk(dir: &Path, out: &mut Vec<Candidate>) {
        let Ok(ents) = fs::read_dir(dir) else { return };
        for e in ents.flatten() {
            let full = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                walk(&full, out);
            } else if e.file_type().map(|t| t.is_file()).unwrap_or(false)
                && name.starts_with("rollout-")
                && name.ends_with(".jsonl")
            {
                out.push(Candidate {
                    tool: "codex",
                    id: None,
                    last_activity_ms: mtime_ms(&full),
                    file: full,
                });
            }
        }
    }
    walk(&codex_root(), &mut out);
    out
}

pub struct Options<'a> {
    pub active_within_min: f64,
    pub tool: Option<&'a str>,
    pub exclude_id: Option<&'a str>,
    pub cwd: Option<&'a str>,
    pub limit: usize,
}

impl Default for Options<'_> {
    fn default() -> Self {
        Options {
            active_within_min: 60.0,
            tool: None,
            exclude_id: None,
            cwd: None,
            limit: 50,
        }
    }
}

/// One discovered session, as the JSON object the bus/CLI emit.
pub fn discover(opts: &Options) -> Vec<JsonValue> {
    let now = store::now_ms();
    let cutoff = now - (opts.active_within_min * 60_000.0) as i64;

    // 1) cheap stat pass: enumerate + window-filter BEFORE reading any content.
    let mut files: Vec<Candidate> = list_claude_files()
        .into_iter()
        .chain(list_codex_files())
        .filter(|f| opts.tool.is_none_or(|t| t == f.tool))
        .filter(|f| f.last_activity_ms >= cutoff)
        .collect();
    files.sort_by_key(|f| -f.last_activity_ms); // newest first → first id wins on dedupe

    // 2) content pass: only the windowed survivors get opened/parsed.
    let named: HashMap<String, store::Entry> = store::roster()
        .into_iter()
        .map(|a| (a.id.clone(), a))
        .collect();
    let mut seen: HashSet<String> = HashSet::new();
    let mut rows: Vec<(Option<String>, i64, JsonValue)> = Vec::new(); // (cwd, ageSec, row)
    for f in files {
        let (id, fcwd) = if f.tool == "claude" {
            (f.id.clone(), claude_cwd(&f.file))
        } else {
            match codex_meta(&f.file) {
                Some((id, cwd)) => (id, cwd),
                None => (None, None),
            }
        };
        let Some(id) = id else { continue };
        if !store::is_uuid(&id) {
            continue; // planted/garbage id → skip (and keep it off the doorbell argv)
        }
        if opts.exclude_id.is_some_and(|x| x == id) {
            continue;
        }
        if !seen.insert(id.clone()) {
            continue; // newest-first, so first occurrence wins
        }
        let known = named.get(&id);
        let age_sec = ((now - f.last_activity_ms).max(0) as f64 / 1000.0).round() as i64;
        let cwd = fcwd.or_else(|| known.and_then(|k| k.dir.clone()));
        let mut m: HashMap<String, JsonValue> = HashMap::new();
        m.insert("tool".into(), JsonValue::from(f.tool.to_string()));
        m.insert("id".into(), JsonValue::from(id));
        m.insert(
            "cwd".into(),
            cwd.clone()
                .map(JsonValue::from)
                .unwrap_or(JsonValue::from(())),
        );
        m.insert(
            "name".into(),
            known
                .and_then(|k| k.name.clone())
                .map(JsonValue::from)
                .unwrap_or(JsonValue::from(())),
        );
        m.insert("registered".into(), JsonValue::from(known.is_some()));
        m.insert(
            "lastActivity".into(),
            JsonValue::from(store::iso_from_unix_ms(f.last_activity_ms)),
        );
        m.insert("ageSec".into(), JsonValue::from(age_sec as f64));
        m.insert("active".into(), JsonValue::from(true)); // window-filtered above
        rows.push((cwd, age_sec, JsonValue::from(m)));
    }
    if let Some(want) = opts.cwd {
        rows.sort_by_key(|(cwd, age, _)| (cwd.as_deref() != Some(want), *age));
    }
    rows.truncate(opts.limit);
    rows.into_iter().map(|(_, _, row)| row).collect()
}
