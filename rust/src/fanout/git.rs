use super::authority::FanoutRecord;
use crate::workspace::git::OpenedRepository;
use crate::workspace::schema::{ObjectFormat, RepositoryIdentityV1};
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

pub(super) fn ensure_worktree_root(path: &Path) -> Result<(), String> {
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err("fanout worktree root must not be a symlink".to_string());
    }
    fs::create_dir_all(path)
        .map_err(|error| format!("create fanout worktree root {}: {error}", path.display()))?;
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("stat fanout worktree root {}: {error}", path.display()))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err("fanout worktree root is not a real directory".to_string());
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
