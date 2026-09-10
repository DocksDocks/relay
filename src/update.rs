// update.rs — `relay update`: replace the running executable with a published
// release build for this target, after checking the download against the
// release `SHA256SUMS`.
//
//   relay update                      install the latest release
//   relay update --check              print the running and latest versions
//   relay update --version <tag>      install a named release
//   relay update --version <tag> --force   allow a downgrade
//
// The release source is fixed at compile time. No environment variable moves
// it, because one redirected value would point both the artifact and its
// checksum at the same attacker and defeat the only integrity control.

use crate::sha256::{Sha256, constant_time_eq};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tinyjson::JsonValue;

const API_LATEST: &str = "https://api.github.com/repos/DocksDocks/relay/releases/latest";
const API_TAGS: &str = "https://api.github.com/repos/DocksDocks/relay/releases/tags/";
const ASSET_HOST: &str = "github.com";
const SUMS_ASSET: &str = "SHA256SUMS";
const USER_AGENT: &str = concat!("relay/", env!("CARGO_PKG_VERSION"));
const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";
// Bound for one whole request, body included, so a stalled peer turns into an
// error instead of a hang.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

// The release asset suffix for this build. Every target compiles; a target
// without a published asset refuses at run time, so the gate and CI can build
// the host target without a compile error.
#[cfg(all(target_arch = "x86_64", target_os = "linux", target_env = "musl"))]
pub const TARGET: Option<&str> = Some("x86_64-unknown-linux-musl");
#[cfg(all(target_arch = "aarch64", target_os = "linux", target_env = "musl"))]
pub const TARGET: Option<&str> = Some("aarch64-unknown-linux-musl");
#[cfg(not(any(
    all(target_arch = "x86_64", target_os = "linux", target_env = "musl"),
    all(target_arch = "aarch64", target_os = "linux", target_env = "musl")
)))]
pub const TARGET: Option<&str> = None;

/// One downloadable file of a release.
#[derive(Clone, Debug)]
pub struct Asset {
    pub name: String,
    pub url: String,
}

/// A published release: its tag and the files attached to it.
#[derive(Clone, Debug)]
pub struct Release {
    pub tag: String,
    pub assets: Vec<Asset>,
}

/// Where releases come from. The decision logic runs against this trait, so
/// the unit tests drive the whole command without a network.
pub trait ReleaseSource {
    fn latest(&self) -> Result<Release, String>;
    fn tag(&self, name: &str) -> Result<Release, String>;
    fn fetch(&self, url: &str, sink: &mut dyn Write) -> Result<(), String>;
}

/// What the running version and the selected version imply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Current,
    Upgrade,
    Downgrade,
}

/// The GitHub REST source. The agent is built with `proxy(None)` because
/// `Config::default` reads `HTTPS_PROXY` and friends from the environment,
/// and with a global timeout because the default has none.
pub struct GithubSource {
    agent: ureq::Agent,
}

impl Default for GithubSource {
    fn default() -> Self {
        let config = ureq::config::Config::builder()
            .proxy(None)
            .timeout_global(Some(REQUEST_TIMEOUT))
            .build();
        Self {
            agent: ureq::Agent::new_with_config(config),
        }
    }
}

impl GithubSource {
    fn release(&self, url: &str) -> Result<Release, String> {
        let mut response = self
            .agent
            .get(url)
            .header("User-Agent", USER_AGENT)
            .header("Accept", "application/vnd.github+json")
            .call()
            .map_err(|error| format!("GET {url} failed: {error}"))?;
        let text = response
            .body_mut()
            .read_to_string()
            .map_err(|error| format!("GET {url} failed: {error}"))?;
        parse_release(&text)
    }
}

impl ReleaseSource for GithubSource {
    fn latest(&self) -> Result<Release, String> {
        self.release(API_LATEST)
    }

    fn tag(&self, name: &str) -> Result<Release, String> {
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+'))
        {
            return Err(format!("bad release tag: {name}"));
        }
        self.release(&format!("{API_TAGS}{name}"))
    }

    fn fetch(&self, url: &str, mut sink: &mut dyn Write) -> Result<(), String> {
        let mut response = self
            .agent
            .get(url)
            .header("User-Agent", USER_AGENT)
            .call()
            .map_err(|error| format!("GET {url} failed: {error}"))?;
        let mut reader = response.body_mut().as_reader();
        std::io::copy(&mut reader, &mut sink)
            .map_err(|error| format!("GET {url} failed: {error}"))?;
        Ok(())
    }
}

fn parse_release(text: &str) -> Result<Release, String> {
    let value: JsonValue = text
        .parse()
        .map_err(|error| format!("release JSON did not parse: {error}"))?;
    let JsonValue::Object(object) = value else {
        return Err("release JSON is not an object".to_string());
    };
    let Some(JsonValue::String(tag)) = object.get("tag_name") else {
        return Err("release JSON has no tag_name".to_string());
    };
    let Some(JsonValue::Array(entries)) = object.get("assets") else {
        return Err("release JSON has no assets".to_string());
    };
    let mut assets = Vec::with_capacity(entries.len());
    for entry in entries {
        let JsonValue::Object(fields) = entry else {
            continue;
        };
        let (Some(JsonValue::String(name)), Some(JsonValue::String(url))) =
            (fields.get("name"), fields.get("browser_download_url"))
        else {
            continue;
        };
        assets.push(Asset {
            name: name.clone(),
            url: url.clone(),
        });
    }
    Ok(Release {
        tag: tag.clone(),
        assets,
    })
}

/// The host part of an `https` URL, without userinfo handling: a URL that
/// carries anything but a plain host fails the exact-match check below.
fn host_of(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("https://")?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    Some(&rest[..end])
}

fn check_host(url: &str) -> Result<(), String> {
    match host_of(url) {
        Some(host) if host == ASSET_HOST => Ok(()),
        Some(host) => Err(format!("unexpected asset host: {host}")),
        None => Err(format!("unexpected asset host: {url}")),
    }
}

/// `X.Y.Z` or `vX.Y.Z` as three numbers, so ordering is numeric.
pub fn parse_version(text: &str) -> Result<(u64, u64, u64), String> {
    let core = without_v(text.trim());
    let mut parts = core.split('.');
    let mut numbers = [0u64; 3];
    for slot in &mut numbers {
        let part = parts
            .next()
            .ok_or_else(|| format!("not a version: {text}"))?;
        *slot = part
            .parse::<u64>()
            .map_err(|_| format!("not a version: {text}"))?;
    }
    if parts.next().is_some() {
        return Err(format!("not a version: {text}"));
    }
    Ok((numbers[0], numbers[1], numbers[2]))
}

fn without_v(text: &str) -> &str {
    text.strip_prefix('v').unwrap_or(text)
}

/// Compare the running version with the selected one. A downgrade needs
/// `--force`; anything that does not parse fails the command.
pub fn plan_update(running: &str, selected: &str, force: bool) -> Result<Decision, String> {
    let current = parse_version(running)?;
    let wanted = parse_version(selected)?;
    if wanted == current {
        return Ok(Decision::Current);
    }
    if wanted > current {
        return Ok(Decision::Upgrade);
    }
    if force {
        return Ok(Decision::Downgrade);
    }
    Err(format!(
        "refusing to downgrade relay {} to {}",
        without_v(running.trim()),
        without_v(selected.trim())
    ))
}

/// Check one `sha256sum` manifest line against the digest of the download.
pub fn verify(sums: &str, asset: &str, digest_hex: &str) -> Result<(), String> {
    for line in sums.lines() {
        let Some((expected, name)) = line.split_once("  ") else {
            continue;
        };
        if name.trim() != asset {
            continue;
        }
        let expected = expected.trim();
        return if constant_time_eq(expected.as_bytes(), digest_hex.as_bytes()) {
            Ok(())
        } else {
            Err(format!(
                "checksum mismatch for {asset}: manifest {expected}, download {digest_hex}"
            ))
        };
    }
    Err(format!("no checksum for {asset}"))
}

/// Run the staged file and require it to report the selected version. This is
/// the last check before the rename; a mismatch removes the staged file.
pub fn verify_staged(staged: &Path, expected_version: &str) -> Result<(), String> {
    let expected = format!("relay {}", without_v(expected_version.trim()));
    let output = match std::process::Command::new(staged).arg("--version").output() {
        Ok(output) => output,
        Err(error) => {
            let _ = fs::remove_file(staged);
            return Err(format!(
                "staged binary {} did not run: {error}",
                staged.display()
            ));
        }
    };
    let reported = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if reported == expected {
        return Ok(());
    }
    let _ = fs::remove_file(staged);
    Err(format!("staged binary reports {reported}"))
}

/// The message for anything that stops a staged file from landing: the path
/// the user has to fix, then the system reason.
fn not_writable(path: &Path) -> impl Fn(std::io::Error) -> String + '_ {
    move |error| format!("{} is not writable: {error}", path.display())
}

/// Move the staged file over the running executable. On Unix the rename is
/// atomic and the running process keeps its old inode. The mode is set here
/// as well as before the download, so a direct caller gets an executable
/// target without staging one.
#[cfg(unix)]
pub fn replace_executable(target: &Path, staged: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    let directory = target
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", target.display()))?;
    if staged.parent() != Some(directory) {
        return Err(format!(
            "staged file {} is not beside {}",
            staged.display(),
            target.display()
        ));
    }
    let outcome = (|| {
        let file = File::open(staged).map_err(not_writable(staged))?;
        file.set_permissions(fs::Permissions::from_mode(0o755))
            .map_err(not_writable(staged))?;
        file.sync_all().map_err(not_writable(staged))?;
        drop(file);
        fs::rename(staged, target).map_err(not_writable(target))?;
        File::open(directory)
            .and_then(|handle| handle.sync_all())
            .map_err(not_writable(directory))
    })();
    if outcome.is_err() {
        let _ = fs::remove_file(staged);
    }
    outcome
}

#[cfg(not(unix))]
pub fn replace_executable(_target: &Path, _staged: &Path) -> Result<(), String> {
    Err("self-replacement is not implemented for this platform".to_string())
}

struct Options {
    check: bool,
    force: bool,
    version: Option<String>,
}

fn parse_args(args: &[String]) -> Result<Options, String> {
    let mut options = Options {
        check: false,
        force: false,
        version: None,
    };
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--check" => options.check = true,
            "--force" => options.force = true,
            "--version" => {
                index += 1;
                let Some(tag) = args.get(index) else {
                    return Err("--version needs a release tag".to_string());
                };
                options.version = Some(tag.clone());
            }
            other => return Err(format!("unknown update option: {other}")),
        }
        index += 1;
    }
    if options.force && options.version.is_none() {
        return Err("--force requires --version <tag>".to_string());
    }
    Ok(options)
}

/// Tees the download into the staged file and into the digest at once, so the
/// body is never held in memory.
struct HashingSink<'a> {
    hasher: Sha256,
    file: &'a mut File,
}

impl Write for HashingSink<'_> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let written = self.file.write(buffer)?;
        self.hasher.update(&buffer[..written]);
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

fn hex(bytes: &[u8; 32]) -> String {
    let mut text = String::with_capacity(64);
    for byte in bytes {
        text.push(char::from(HEX_DIGITS[usize::from(byte >> 4)]));
        text.push(char::from(HEX_DIGITS[usize::from(byte & 0x0f)]));
    }
    text
}

fn staged_path(exe: &Path) -> Result<PathBuf, String> {
    let name = exe
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("cannot stage beside {}", exe.display()))?;
    Ok(exe.with_file_name(format!("{name}.update-{}", std::process::id())))
}

fn find_asset<'a>(release: &'a Release, name: &str) -> Result<&'a Asset, String> {
    release
        .assets
        .iter()
        .find(|asset| asset.name == name)
        .ok_or_else(|| format!("release {} has no asset {name}", release.tag))
}

fn write_line(out: &mut dyn Write, line: &str) -> Result<(), String> {
    writeln!(out, "{line}").map_err(|error| format!("cannot write output: {error}"))
}

/// Download the asset and the manifest into the staged file and check the
/// digest. The caller owns the staged file and removes it on any error.
fn stage(
    source: &dyn ReleaseSource,
    asset: &Asset,
    sums: &Asset,
    asset_name: &str,
    file: &mut File,
    staged: &Path,
) -> Result<(), String> {
    let digest = {
        let mut sink = HashingSink {
            hasher: Sha256::new(),
            file,
        };
        source.fetch(&asset.url, &mut sink)?;
        sink.flush().map_err(not_writable(staged))?;
        hex(&sink.hasher.digest())
    };
    file.sync_all().map_err(not_writable(staged))?;
    let mut manifest = Vec::new();
    source.fetch(&sums.url, &mut manifest)?;
    let text =
        String::from_utf8(manifest).map_err(|_| format!("{SUMS_ASSET} is not UTF-8 text"))?;
    verify(&text, asset_name, &digest)
}

#[cfg(unix)]
fn mark_executable(file: &File, staged: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o755))
        .map_err(not_writable(staged))
}

#[cfg(not(unix))]
fn mark_executable(_file: &File, _staged: &Path) -> Result<(), String> {
    Ok(())
}

fn install(
    source: &dyn ReleaseSource,
    release: &Release,
    target: &str,
    exe: &Path,
    out: &mut dyn Write,
) -> Result<(), String> {
    let asset_name = format!("relay-{target}");
    let asset = find_asset(release, &asset_name)?;
    let sums = find_asset(release, SUMS_ASSET)?;
    check_host(&asset.url)?;
    check_host(&sums.url)?;

    let staged = staged_path(exe)?;
    let mut file = File::create(&staged).map_err(not_writable(exe))?;
    let staged_result = mark_executable(&file, &staged)
        .and_then(|()| stage(source, asset, sums, &asset_name, &mut file, &staged));
    drop(file);
    let outcome = staged_result
        .and_then(|()| verify_staged(&staged, &release.tag))
        .and_then(|()| replace_executable(exe, &staged));
    if outcome.is_err() {
        let _ = fs::remove_file(&staged);
        return outcome;
    }
    write_line(
        out,
        &format!("relay updated to {}", without_v(&release.tag)),
    )
}

/// The whole command without process exit, so the unit tests can drive it.
pub fn execute(
    args: &[String],
    source: &dyn ReleaseSource,
    running: &str,
    target: Option<&str>,
    exe: &Path,
    out: &mut dyn Write,
) -> Result<(), String> {
    let options = parse_args(args)?;
    let Some(target) = target else {
        return Err("no release asset for this target".to_string());
    };
    let release = match &options.version {
        Some(tag) => source.tag(tag)?,
        None => source.latest()?,
    };
    if options.check {
        let label = if options.version.is_some() {
            "selected"
        } else {
            "latest"
        };
        write_line(out, &format!("relay {running}"))?;
        return write_line(out, &format!("{label}: {}", release.tag));
    }
    match plan_update(running, &release.tag, options.force)? {
        Decision::Current => write_line(out, &format!("relay {running} is current")),
        Decision::Upgrade | Decision::Downgrade => install(source, &release, target, exe, out),
    }
}

pub fn run(args: &[String]) -> ! {
    let exe = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => die(&format!("cannot find the running executable: {error}")),
    };
    let source = GithubSource::default();
    let mut out = std::io::stdout();
    match execute(
        args,
        &source,
        env!("CARGO_PKG_VERSION"),
        TARGET,
        &exe,
        &mut out,
    ) {
        Ok(()) => match out.flush() {
            Ok(()) => std::process::exit(0),
            Err(error) => die(&format!("cannot write output: {error}")),
        },
        Err(message) => die(&message),
    }
}

fn die(message: &str) -> ! {
    eprintln!("{message}");
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sha256::hex_digest;
    use crate::store::uuid_v4;
    use std::cell::RefCell;
    use std::collections::HashMap;

    const TEST_TARGET: Option<&str> = Some("x86_64-unknown-linux-musl");
    const ASSET_NAME: &str = "relay-x86_64-unknown-linux-musl";

    struct FakeSource {
        tag: String,
        assets: Vec<Asset>,
        bodies: HashMap<String, Vec<u8>>,
        fetched: RefCell<Vec<String>>,
    }

    impl ReleaseSource for FakeSource {
        fn latest(&self) -> Result<Release, String> {
            Ok(Release {
                tag: self.tag.clone(),
                assets: self.assets.clone(),
            })
        }

        fn tag(&self, name: &str) -> Result<Release, String> {
            Ok(Release {
                tag: name.to_string(),
                assets: self.assets.clone(),
            })
        }

        fn fetch(&self, url: &str, sink: &mut dyn Write) -> Result<(), String> {
            self.fetched.borrow_mut().push(url.to_string());
            let body = self
                .bodies
                .get(url)
                .ok_or_else(|| format!("no body for {url}"))?;
            sink.write_all(body).map_err(|error| error.to_string())
        }
    }

    // A payload that behaves like a relay binary for the `--version` check.
    fn payload(version: &str) -> Vec<u8> {
        format!("#!/bin/sh\necho 'relay {version}'\n").into_bytes()
    }

    fn source(tag: &str, body: &[u8], digest: &str, host: &str) -> FakeSource {
        let base = format!("https://{host}/DocksDocks/relay/releases/download/{tag}");
        let asset_url = format!("{base}/{ASSET_NAME}");
        let sums_url = format!("{base}/{SUMS_ASSET}");
        let mut bodies = HashMap::new();
        bodies.insert(asset_url.clone(), body.to_vec());
        bodies.insert(
            sums_url.clone(),
            format!("{digest}  {ASSET_NAME}\n").into_bytes(),
        );
        FakeSource {
            tag: tag.to_string(),
            assets: vec![
                Asset {
                    name: ASSET_NAME.to_string(),
                    url: asset_url,
                },
                Asset {
                    name: SUMS_ASSET.to_string(),
                    url: sums_url,
                },
            ],
            bodies,
            fetched: RefCell::new(Vec::new()),
        }
    }

    // A stand-in for the running executable in its own directory. The
    // directory goes away with the value, so a test run leaves no scratch.
    struct Fixture(PathBuf);

    impl std::ops::Deref for Fixture {
        type Target = Path;

        fn deref(&self) -> &Path {
            &self.0
        }
    }

    impl AsRef<Path> for Fixture {
        fn as_ref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            if let Some(root) = self.0.parent() {
                let _ = fs::remove_dir_all(root);
            }
        }
    }

    fn fixture(label: &str) -> Fixture {
        let root = std::env::temp_dir().join(format!("relay-update-{label}-{}", uuid_v4()));
        fs::create_dir_all(&root).expect("create update fixture");
        let exe = root.join("relay");
        fs::write(&exe, payload("0.1.0")).expect("write fixture executable");
        Fixture(exe)
    }

    fn text(out: &[u8]) -> String {
        String::from_utf8(out.to_vec()).expect("output is text")
    }

    #[test]
    fn fresh_release_replaces_the_executable() {
        let exe = fixture("fresh");
        let body = payload("0.2.0");
        let source = source("v0.2.0", &body, &hex_digest(&body), ASSET_HOST);
        let mut out = Vec::new();

        execute(&[], &source, "0.1.0", TEST_TARGET, &exe, &mut out).expect("update succeeds");

        assert_eq!(fs::read(&exe).expect("read replaced file"), body);
        assert_eq!(text(&out), "relay updated to 0.2.0\n");
        assert!(!staged_path(&exe).expect("staged path").exists());
    }

    #[test]
    fn equal_version_changes_nothing() {
        let exe = fixture("equal");
        let before = fs::read(&exe).expect("read fixture");
        let body = payload("0.2.0");
        let source = source("v0.2.0", &body, &hex_digest(&body), ASSET_HOST);
        let mut out = Vec::new();

        execute(&[], &source, "0.2.0", TEST_TARGET, &exe, &mut out).expect("current is a success");

        assert_eq!(text(&out), "relay 0.2.0 is current\n");
        assert_eq!(fs::read(&exe).expect("read fixture"), before);
        assert!(source.fetched.borrow().is_empty());
    }

    #[test]
    fn older_release_is_refused_without_force() {
        let exe = fixture("downgrade");
        let before = fs::read(&exe).expect("read fixture");
        let body = payload("0.2.0");
        let source = source("v0.2.0", &body, &hex_digest(&body), ASSET_HOST);
        let mut out = Vec::new();

        let error = execute(&[], &source, "0.3.0", TEST_TARGET, &exe, &mut out)
            .expect_err("downgrade is refused");

        assert_eq!(error, "refusing to downgrade relay 0.3.0 to 0.2.0");
        assert_eq!(fs::read(&exe).expect("read fixture"), before);
        assert!(source.fetched.borrow().is_empty());
    }

    #[test]
    fn forced_version_downgrades() {
        let exe = fixture("forced");
        let body = payload("0.2.0");
        let source = source("v0.2.0", &body, &hex_digest(&body), ASSET_HOST);
        let args = [
            "--version".to_string(),
            "v0.2.0".to_string(),
            "--force".to_string(),
        ];
        let mut out = Vec::new();

        execute(&args, &source, "0.3.0", TEST_TARGET, &exe, &mut out).expect("forced downgrade");

        assert_eq!(fs::read(&exe).expect("read replaced file"), body);
        assert_eq!(text(&out), "relay updated to 0.2.0\n");
    }

    #[test]
    fn checksum_mismatch_keeps_the_executable() {
        let exe = fixture("checksum");
        let before = fs::read(&exe).expect("read fixture");
        let body = payload("0.2.0");
        let source = source("v0.2.0", &body, &hex_digest(b"other bytes"), ASSET_HOST);
        let mut out = Vec::new();

        let error = execute(&[], &source, "0.1.0", TEST_TARGET, &exe, &mut out)
            .expect_err("mismatch fails the update");

        assert!(error.starts_with("checksum mismatch for relay-"), "{error}");
        assert_eq!(fs::read(&exe).expect("read fixture"), before);
        assert!(!staged_path(&exe).expect("staged path").exists());
    }

    #[test]
    fn check_reports_versions_without_downloading() {
        let exe = fixture("check");
        let body = payload("0.2.0");
        let source = source("v0.2.0", &body, &hex_digest(&body), ASSET_HOST);
        let mut out = Vec::new();

        execute(
            &["--check".to_string()],
            &source,
            "0.1.0",
            TEST_TARGET,
            &exe,
            &mut out,
        )
        .expect("check succeeds");

        assert_eq!(text(&out), "relay 0.1.0\nlatest: v0.2.0\n");
        assert!(source.fetched.borrow().is_empty());
    }

    #[test]
    fn check_labels_a_selected_tag() {
        let exe = fixture("check-selected");
        let body = payload("0.2.0");
        let source = source("v0.2.0", &body, &hex_digest(&body), ASSET_HOST);
        let mut out = Vec::new();

        execute(
            &[
                "--check".to_string(),
                "--version".to_string(),
                "v0.2.0".to_string(),
            ],
            &source,
            "0.1.0",
            TEST_TARGET,
            &exe,
            &mut out,
        )
        .expect("check succeeds");

        assert_eq!(text(&out), "relay 0.1.0\nselected: v0.2.0\n");
        assert!(source.fetched.borrow().is_empty());
    }

    #[test]
    fn foreign_asset_host_is_refused() {
        let exe = fixture("host");
        let body = payload("0.2.0");
        let source = source("v0.2.0", &body, &hex_digest(&body), "evil.example.com");
        let mut out = Vec::new();

        let error = execute(&[], &source, "0.1.0", TEST_TARGET, &exe, &mut out)
            .expect_err("foreign host is refused");

        assert_eq!(error, "unexpected asset host: evil.example.com");
        assert!(source.fetched.borrow().is_empty());
    }

    #[test]
    fn unparsable_tag_is_refused() {
        let exe = fixture("tag");
        let body = payload("0.2.0");
        let source = source("v0.2.0", &body, &hex_digest(&body), ASSET_HOST);
        let args = ["--version".to_string(), "nightly".to_string()];
        let mut out = Vec::new();

        let error = execute(&args, &source, "0.1.0", TEST_TARGET, &exe, &mut out)
            .expect_err("a tag that is not a version fails");

        assert_eq!(error, "not a version: nightly");
        assert!(source.fetched.borrow().is_empty());
    }
}
