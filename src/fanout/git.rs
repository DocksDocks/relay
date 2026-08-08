use super::authority::FanoutRecord;
use crate::sha256;
use crate::workspace::git::OpenedRepository;
use crate::workspace::schema::{ObjectFormat, RepositoryIdentityV1};
use rustix::fs::{CWD, Mode, OFlags, mkdirat, openat};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct RepoIdentity {
    pub(super) common_dir: String,
    pub(super) dev: String,
    pub(super) ino: String,
    pub(super) object_format: ObjectFormat,
    pub(super) repository_id: String,
    workspace: RepositoryIdentityV1,
}

impl RepoIdentity {
    pub(super) fn matches_record(&self, record: &FanoutRecord) -> bool {
        self.common_dir == record.repo_common_dir
            && self.dev == record.repo_dev
            && self.ino == record.repo_ino
            && record
                .object_format
                .as_deref()
                .is_none_or(|format| format == self.object_format.as_str())
    }

    pub(super) fn workspace_identity(&self) -> &RepositoryIdentityV1 {
        &self.workspace
    }
}

pub(super) enum PreparedMergeOutcome {
    Merged,
    Aborted { merge_error: String },
}

pub(super) fn canonicalize_repository(repo: &Path) -> Result<PathBuf, String> {
    OpenedRepository::open(repo).map(|opened| opened.root)
}

pub(super) fn repo_identity(repo: &Path) -> Result<RepoIdentity, String> {
    let opened = OpenedRepository::open(repo)?;
    let workspace = opened.identity;
    Ok(RepoIdentity {
        common_dir: workspace.common_dir_realpath.clone(),
        dev: workspace.common_dir_dev.clone(),
        ino: workspace.common_dir_ino.clone(),
        object_format: workspace.object_format,
        repository_id: workspace.repository_id.clone(),
        workspace,
    })
}

const EMPTY_REPOSITORY_KEY: &str = "no-commits";
const INVALID_WORKTREE_ROOT_COMPONENT: &str =
    "fanout worktree root component is a symlink or not a directory";

/// Return a repo-key containing at most two slash-separated components.
///
/// A usable remote contributes its final `owner/repo` components. Remotes whose
/// path cannot be represented unchanged in the worktree-component charset fall
/// back to a SHA-256 digest of the repository's sorted root commits.
pub(super) fn repo_key_from_repo(repo: &Path) -> Result<String, String> {
    if let Ok(remote) = run_git(repo, &["remote", "get-url", "origin"]) {
        if let Some(key) = repo_key_from_remote(&remote) {
            return Ok(key);
        }
    }

    let roots = run_git(
        repo,
        &["rev-list", "--max-parents=0", "--ignore-missing", "HEAD"],
    )?;
    let mut roots: Vec<&str> = roots
        .lines()
        .map(str::trim)
        .filter(|oid| !oid.is_empty())
        .collect();
    if roots.is_empty() {
        return Ok(EMPTY_REPOSITORY_KEY.to_string());
    }
    roots.sort_unstable();
    Ok(sha256::hex_digest(roots.join("\n").as_bytes()))
}

fn repo_key_from_remote(remote: &str) -> Option<String> {
    let remote = remote.trim();
    let path = if let Some((scheme, remainder)) = remote.split_once("://") {
        if scheme.is_empty() || scheme.eq_ignore_ascii_case("file") {
            return None;
        }
        let (authority, path) = remainder.split_once('/')?;
        if authority.is_empty() {
            return None;
        }
        path
    } else {
        let separator = scp_path_separator(remote)?;
        if separator == 0 {
            return None;
        }
        &remote[separator + 1..]
    };

    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut segments: Vec<String> = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .rev()
        .take(2)
        .map(str::to_ascii_lowercase)
        .collect();
    segments.reverse();
    if segments.is_empty()
        || segments
            .iter()
            .any(|segment| !valid_repo_key_segment(segment))
    {
        return None;
    }
    Some(segments.join("/"))
}

fn scp_path_separator(remote: &str) -> Option<usize> {
    let mut bracketed_host = false;
    for (index, character) in remote.char_indices() {
        match character {
            '[' => bracketed_host = true,
            ']' => bracketed_host = false,
            ':' if !bracketed_host => return Some(index),
            '/' if !bracketed_host => return None,
            _ => {}
        }
    }
    None
}

fn valid_repo_key_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment != "."
        && segment != ".."
        && !segment.contains('/')
        && !segment.contains('\\')
        && segment.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-' | b'_')
        })
}

fn open_worktree_directory<Fd: rustix::fd::AsFd>(
    parent: Fd,
    component: &Path,
) -> Result<rustix::fd::OwnedFd, String> {
    match openat(
        parent,
        component,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(directory) => Ok(directory),
        Err(error) if error == rustix::io::Errno::LOOP || error == rustix::io::Errno::NOTDIR => {
            Err(INVALID_WORKTREE_ROOT_COMPONENT.to_string())
        }
        Err(error) => Err(format!(
            "open fanout worktree root component {}: {error}",
            component.display()
        )),
    }
}

fn create_worktree_directory<Fd: rustix::fd::AsFd>(
    parent: Fd,
    component: &Path,
) -> Result<(), String> {
    match mkdirat(parent, component, Mode::from(0o777)) {
        Ok(()) => Ok(()),
        Err(error) if error == rustix::io::Errno::EXIST => Ok(()),
        Err(error) => Err(format!(
            "create fanout worktree root component {}: {error}",
            component.display()
        )),
    }
}

pub(super) fn ensure_worktree_root(worktrees: &Path, repo_key: &str) -> Result<(), String> {
    create_worktree_directory(CWD, worktrees)?;
    let mut directory = open_worktree_directory(CWD, worktrees)?;
    for component in repo_key.split('/') {
        if !valid_repo_key_segment(component) {
            return Err("fanout repo key component is not permitted".to_string());
        }
        let component = Path::new(component);
        create_worktree_directory(&directory, component)?;
        directory = open_worktree_directory(&directory, component)?;
    }
    Ok(())
}

pub(super) fn ensure_clean(repo: &Path, label: &str) -> Result<(), String> {
    let status = run_git(repo, &["status", "--porcelain"])?;
    if status.is_empty() {
        Ok(())
    } else {
        Err(format!("{label} is dirty"))
    }
}

pub(super) fn repository_head(repo: &Path) -> Result<String, String> {
    run_git(repo, &["rev-parse", "--verify", "HEAD"])
}

pub(super) fn changed_paths(
    repo: &Path,
    base_commit: &str,
    handback_commit: &str,
) -> Result<Vec<String>, String> {
    let range = format!("{base_commit}..{handback_commit}");
    let output = run_git_bytes(
        repo,
        &[
            "diff",
            "--name-only",
            "-z",
            "--no-renames",
            "--no-ext-diff",
            "--no-textconv",
            &range,
            "--",
        ],
    )?;
    if output.is_empty() {
        return Ok(Vec::new());
    }
    if !output.ends_with(b"\0") {
        return Err("git changed-path output is not NUL terminated".to_string());
    }
    let mut paths = Vec::new();
    for path in output[..output.len() - 1].split(|byte| *byte == 0) {
        if path.is_empty() {
            return Err("git changed-path output contains an empty path".to_string());
        }
        let path = std::str::from_utf8(path)
            .map_err(|_| "fanout changed path is not UTF-8".to_string())?;
        paths.push(path.to_string());
    }
    paths.sort();
    paths.dedup();
    if paths.len() > 4096 {
        return Err("fanout result has more than 4096 changed paths".to_string());
    }
    Ok(paths)
}

pub(super) fn add_worktree(
    repo: &Path,
    branch: &str,
    worktree: &str,
    base_sha: &str,
) -> Result<(), String> {
    run_git(repo, &["worktree", "add", "-b", branch, worktree, base_sha])?;
    Ok(())
}

pub(super) fn remove_unstarted_worktree(repo: &Path, worktree: &Path) -> Result<(), String> {
    let worktree_arg = worktree.to_string_lossy();
    run_git(repo, &["worktree", "remove", &worktree_arg])?;
    if worktree.exists() {
        return Err("unstarted fanout worktree still exists after removal".to_string());
    }
    Ok(())
}

pub(super) fn validate_sha(value: &str, format: ObjectFormat) -> Result<(), String> {
    crate::workspace::schema::GitOid::parse(value, format).map(|_| ())
}

pub(super) fn merge_prepared_handback(
    parent_dir: &Path,
    worktree: &Path,
    record: &FanoutRecord,
) -> Result<PreparedMergeOutcome, String> {
    ensure_clean(parent_dir, "collect parent")?;
    ensure_clean(worktree, "collect child")?;
    if !repo_identity(worktree)?.matches_record(record) {
        return Err("fanout collect child repository identity changed".to_string());
    }
    let head = record
        .handback_head
        .as_deref()
        .ok_or_else(|| "fanout handback has no head".to_string())?;
    let current_head = repository_head(worktree)?;
    if current_head != head {
        return Err(format!(
            "fanout child HEAD changed after handback; restore {} to {head} before retrying collection",
            worktree.display()
        ));
    }
    if let Err(merge_error) = run_git(parent_dir, &["merge", "--no-ff", "--no-edit", head]) {
        let abort = run_git(parent_dir, &["merge", "--abort"]);
        let clean = ensure_clean(parent_dir, "collect parent after merge abort");
        if abort.is_ok() && clean.is_ok() {
            return Ok(PreparedMergeOutcome::Aborted { merge_error });
        }
        return Err(format!(
            "merge failed and abort could not restore a clean parent at {}; run `git merge --abort`, clean the checkout, then retry: {merge_error}",
            parent_dir.display()
        ));
    }
    Ok(PreparedMergeOutcome::Merged)
}

pub(super) fn remove_merged_worktree(
    parent_dir: &Path,
    worktree: &Path,
    record: &FanoutRecord,
) -> Result<(), String> {
    let tracked = git_tracks_worktree(parent_dir, worktree)?;
    match fs::symlink_metadata(worktree) {
        Ok(_) => {
            if !tracked {
                return Err("fanout collect child exists but is no longer registered".to_string());
            }
            ensure_clean(worktree, "collect child")?;
            if !repo_identity(worktree)?.matches_record(record) {
                return Err("fanout collect child repository identity changed".to_string());
            }
            run_git(
                parent_dir,
                &["worktree", "remove", record.worktree.as_str()],
            )?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && !tracked => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!(
                "fanout worktree {} is missing but remains registered; repair the git worktree metadata before retrying",
                worktree.display()
            ));
        }
        Err(error) => {
            return Err(format!(
                "inspect fanout worktree {}: {error}",
                worktree.display()
            ));
        }
    }
    Ok(())
}

/// Commits on `branch` that are absent from `base_sha`. Routed through `run_git`
/// like every other git call in this module: spawning the git binary directly
/// here would register a new direct-git site, which the frozen reentry inventory
/// classifies separately from the sanctioned git-API category.
pub(super) fn uncollected_commit_count(
    worktree: &Path,
    base_sha: &str,
    branch: &str,
) -> Result<u64, String> {
    let range = format!("{base_sha}..{branch}");
    run_git(worktree, &["rev-list", "--count", &range])?
        .parse()
        .map_err(|_| format!("git rev-list count for {branch} was not an integer"))
}

fn run_git(cwd: &Path, args: &[&str]) -> Result<String, String> {
    String::from_utf8(run_git_bytes(cwd, args)?)
        .map(|output| output.trim().to_string())
        .map_err(|_| "git output was not UTF-8".to_string())
}

fn run_git_bytes(cwd: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|error| format!("run git {}: {error}", args.join(" ")))?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed in {}: {}",
            args.join(" "),
            cwd.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output.stdout)
}

fn git_tracks_worktree(repo: &Path, worktree: &Path) -> Result<bool, String> {
    let expected = worktree.to_string_lossy();
    Ok(run_git(repo, &["worktree", "list", "--porcelain"])?
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .any(|registered| registered == expected))
}
