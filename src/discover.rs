// discover.rs — find agent sessions running RIGHT NOW by scanning the raw
// on-disk session stores (port of lib/discover.mjs), so the bus can
// auto-resolve "my other session" with NO prior bus registration.
//   omp:    <root>/<cwd-bucket>/*.jsonl — a session header in the first two
//           physical lines carries id + cwd; bucket names are not decoded.
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

/// Resolve omp's session store without reading the environment or filesystem.
/// `PWD` supplies the absolute working directory for relative agent overrides.
pub(crate) fn omp_sessions_root(
    get: &dyn Fn(&str) -> Option<String>,
    exists: &dyn Fn(&Path) -> bool,
) -> PathBuf {
    if let Some(root) = get("RELAY_OMP_SESSIONS").filter(|value| !value.is_empty()) {
        return PathBuf::from(root);
    }
    let profile = get("OMP_PROFILE").or_else(|| get("PI_PROFILE"));
    let profile = profile
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty() && *value != "default");
    let home = PathBuf::from(get("HOME").unwrap_or_else(|| ".".into()));
    let config = get("PI_CONFIG_DIR")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| ".omp".into());
    // Node's path.join keeps even an absolute-looking config name under HOME.
    let mut config_root = home.join(config.trim_start_matches('/'));
    if let Some(profile) = profile {
        config_root = config_root.join("profiles").join(profile);
    }
    let default_agent = normalize_path(&config_root.join("agent"));
    let agent_dir = if let Some(agent) = get("PI_CODING_AGENT_DIR").filter(|_| profile.is_none()) {
        let agent = PathBuf::from(agent);
        let absolute = if agent.is_absolute() {
            agent
        } else {
            PathBuf::from(get("PWD").unwrap_or_else(|| "/".into())).join(agent)
        };
        normalize_path(&absolute)
    } else {
        default_agent.clone()
    };
    if agent_dir == default_agent {
        if let Some(xdg) = get("XDG_DATA_HOME").filter(|value| !value.is_empty()) {
            let mut candidate = PathBuf::from(xdg).join("omp");
            if let Some(profile) = profile {
                candidate = candidate.join("profiles").join(profile);
            }
            let candidate = normalize_path(&candidate);
            if exists(&candidate) {
                return candidate.join("sessions");
            }
        }
    }
    agent_dir.join("sessions")
}

// Match Node path normalization without requiring the target to exist.
fn normalize_path(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if normalized.file_name().is_some_and(|name| name != "..") {
                    normalized.pop();
                } else if !normalized.has_root() {
                    normalized.push("..");
                }
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    if normalized.as_os_str().is_empty() {
        normalized.push(".");
    }
    normalized
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

// omp may prepend a title record before its session header.
fn omp_meta(file: &Path) -> Option<(Option<String>, Option<String>)> {
    for line in head_lines(file).into_iter().take(2) {
        let Ok(value) = line.parse::<JsonValue>() else {
            continue;
        };
        let Some(header) = as_obj(&value) else {
            continue;
        };
        if header
            .get("type")
            .and_then(|value| value.get::<String>())
            .map(String::as_str)
            == Some("session")
        {
            return Some((str_field(header, "id"), str_field(header, "cwd")));
        }
    }
    None
}

struct Candidate {
    file: PathBuf,
    last_activity_ms: i64,
}

fn list_omp_files(root: &Path) -> Vec<Candidate> {
    let mut out = Vec::new();
    let Ok(buckets) = fs::read_dir(root) else {
        return out;
    };
    for bucket in buckets.flatten() {
        if !bucket.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let Ok(entries) = fs::read_dir(bucket.path()) else {
            continue;
        };
        for entry in entries.flatten() {
            let file = entry.path();
            if entry.file_type().map(|t| t.is_file()).unwrap_or(false)
                && file.extension().is_some_and(|ext| ext == "jsonl")
            {
                out.push(Candidate {
                    last_activity_ms: mtime_ms(&file),
                    file,
                });
            }
        }
    }
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
    let root = omp_sessions_root(
        &|key| {
            if key == "PWD" {
                std::env::current_dir()
                    .ok()
                    .map(|dir| dir.to_string_lossy().into_owned())
            } else {
                std::env::var(key).ok()
            }
        },
        &Path::exists,
    );
    discover_files(opts, list_omp_files(&root).into_iter())
}

fn discover_files(opts: &Options, files: impl Iterator<Item = Candidate>) -> Vec<JsonValue> {
    let now = store::now_ms();
    let cutoff = now - (opts.active_within_min * 60_000.0) as i64;

    // 1) cheap stat pass: enumerate + window-filter BEFORE reading any content.
    let mut files: Vec<Candidate> = files
        .filter(|_| opts.tool.is_none_or(|tool| tool == "omp"))
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
        let (id, fcwd) = omp_meta(&f.file).unwrap_or((None, None));
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
        m.insert("tool".into(), JsonValue::from("omp".to_string()));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn omp_root(vars: &[(&str, &str)], existing: &[&str]) -> PathBuf {
        let env: HashMap<_, _> = vars.iter().copied().collect();
        omp_sessions_root(
            &|key| env.get(key).map(|value| (*value).to_string()),
            &|path| existing.iter().any(|existing| path == Path::new(existing)),
        )
    }

    #[test]
    fn omp_root_relay_override_wins() {
        assert_eq!(
            omp_root(
                &[
                    ("HOME", "/home/test"),
                    ("RELAY_OMP_SESSIONS", "/relay/sessions"),
                    ("OMP_PROFILE", "work"),
                    ("XDG_DATA_HOME", "/data"),
                ],
                &["/data/omp/profiles/work"],
            ),
            PathBuf::from("/relay/sessions"),
        );
    }

    #[test]
    fn omp_root_named_profile_ignores_agent_override() {
        assert_eq!(
            omp_root(
                &[
                    ("HOME", "/home/test"),
                    ("OMP_PROFILE", " work "),
                    ("PI_CODING_AGENT_DIR", "/custom/agent"),
                ],
                &[],
            ),
            PathBuf::from("/home/test/.omp/profiles/work/agent/sessions"),
        );
    }

    #[test]
    fn omp_root_empty_omp_profile_beats_pi_profile() {
        assert_eq!(
            omp_root(
                &[
                    ("HOME", "/home/test"),
                    ("OMP_PROFILE", ""),
                    ("PI_PROFILE", "work"),
                    ("PI_CODING_AGENT_DIR", "/custom/agent"),
                ],
                &[],
            ),
            PathBuf::from("/custom/agent/sessions"),
        );
    }

    #[test]
    fn omp_root_config_directory_stays_relative_to_home() {
        for config in ["custom/config", "/custom/config"] {
            assert_eq!(
                omp_root(&[("HOME", "/home/test"), ("PI_CONFIG_DIR", config)], &[]),
                PathBuf::from("/home/test/custom/config/agent/sessions"),
            );
        }
    }

    #[test]
    fn omp_root_xdg_requires_existing_profile_specific_root() {
        let vars = [
            ("HOME", "/home/test"),
            ("PI_PROFILE", "work"),
            ("XDG_DATA_HOME", "/data"),
        ];
        assert_eq!(
            omp_root(&vars, &["/data/omp"]),
            PathBuf::from("/home/test/.omp/profiles/work/agent/sessions"),
        );
        assert_eq!(
            omp_root(&vars, &["/data/omp/profiles/work"]),
            PathBuf::from("/data/omp/profiles/work/sessions"),
        );
    }

    #[test]
    fn omp_root_existing_xdg_root_only_overrides_default_agent() {
        let mut vars = vec![("HOME", "/home/test"), ("XDG_DATA_HOME", "/data")];
        assert_eq!(
            omp_root(&vars, &["/data/omp"]),
            PathBuf::from("/data/omp/sessions"),
        );
        vars.push(("PI_CODING_AGENT_DIR", "/custom/agent"));
        assert_eq!(
            omp_root(&vars, &["/data/omp"]),
            PathBuf::from("/custom/agent/sessions"),
        );
    }

    #[test]
    fn omp_root_relative_agent_is_resolved_before_xdg_selection() {
        assert_eq!(
            omp_root(
                &[
                    ("HOME", "/home/test"),
                    ("PWD", "/home/test"),
                    ("PI_CODING_AGENT_DIR", ".omp/other/../agent"),
                    ("XDG_DATA_HOME", "/data"),
                ],
                &["/data/omp"],
            ),
            PathBuf::from("/data/omp/sessions"),
        );
    }

    #[test]
    fn omp_root_defaults_when_overrides_are_empty() {
        assert_eq!(
            omp_root(
                &[
                    ("HOME", "/home/test"),
                    ("RELAY_OMP_SESSIONS", ""),
                    ("OMP_PROFILE", "default"),
                    ("PI_CONFIG_DIR", ""),
                    ("XDG_DATA_HOME", ""),
                ],
                &[],
            ),
            PathBuf::from("/home/test/.omp/agent/sessions"),
        );
        assert_eq!(
            omp_root(&[("HOME", "/home/test")], &[]),
            PathBuf::from("/home/test/.omp/agent/sessions"),
        );
    }

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("relay-discover-{}", store::uuid_v4()));
            fs::create_dir_all(root.join("opaque-bucket")).expect("create fixture bucket");
            Self(root)
        }

        fn write(&self, name: &str, text: &str) -> PathBuf {
            let file = self.0.join("opaque-bucket").join(name);
            fs::write(&file, text).expect("write session fixture");
            file
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).expect("remove fixture");
        }
    }

    #[test]
    fn omp_discovery_reads_session_header_after_title() {
        let fixture = Fixture::new();
        let id = "01a081c6-19da-737a-a863-9fb9d50ad5c2";
        fixture.write(
            "timestamp_session.jsonl",
            &format!(
                "{{\"type\":\"title\",\"title\":\"Task\"}}\n\
                 {{\"type\":\"session\",\"id\":\"{id}\",\"cwd\":\"/real/project\"}}\n"
            ),
        );
        let rows = discover_files(&Options::default(), list_omp_files(&fixture.0).into_iter());
        assert_eq!(rows.len(), 1);
        let row = as_obj(&rows[0]).expect("session object");
        assert_eq!(str_field(row, "tool").as_deref(), Some("omp"));
        assert_eq!(str_field(row, "id").as_deref(), Some(id));
        assert_eq!(str_field(row, "cwd").as_deref(), Some("/real/project"));
    }

    #[test]
    fn omp_discovery_limits_header_search_to_two_physical_lines() {
        let fixture = Fixture::new();
        fixture.write(
            "late-header.jsonl",
            "\n{\"type\":\"title\"}\n\
             {\"type\":\"session\",\"id\":\"01a081c6-19da-737a-a863-9fb9d50ad5c2\",\"cwd\":\"/late\"}\n",
        );
        fixture.write(
            "not-a-header.jsonl",
            "{\"type\":\"message\",\"id\":\"01a081c6-19da-737a-a863-9fb9d50ad5c2\",\"cwd\":\"/wrong\"}\n",
        );
        assert!(
            discover_files(&Options::default(), list_omp_files(&fixture.0).into_iter()).is_empty()
        );
    }

    #[test]
    fn omp_discovery_filters_ids_recency_and_duplicate_sessions() {
        let fixture = Fixture::new();
        let id = "01a081c6-19da-737a-a863-9fb9d50ad5c2";
        for (name, id, cwd) in [
            ("older", id, "/older"),
            ("newer", id, "/newer"),
            ("invalid", "not-a-uuid", "/invalid"),
            ("stale", "02a081c6-19da-737a-a863-9fb9d50ad5c2", "/stale"),
        ] {
            fixture.write(
                &format!("{name}.jsonl"),
                &format!("{{\"type\":\"session\",\"id\":\"{id}\",\"cwd\":\"{cwd}\"}}\n"),
            );
        }
        let now = store::now_ms();
        let files = list_omp_files(&fixture.0).into_iter().map(|mut file| {
            file.last_activity_ms = match file.file.file_stem().and_then(|name| name.to_str()) {
                Some("stale") => 0,
                Some("older") => now - 1000,
                _ => now,
            };
            file
        });
        let rows = discover_files(&Options::default(), files);
        assert_eq!(rows.len(), 1);
        let row = as_obj(&rows[0]).expect("session object");
        assert_eq!(str_field(row, "id").as_deref(), Some(id));
        assert_eq!(str_field(row, "cwd").as_deref(), Some("/newer"));
    }
}
