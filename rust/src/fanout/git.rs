use super::authority::FanoutRecord;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct RepoIdentity {
    pub(super) common_dir: String,
    pub(super) dev: String,
    pub(super) ino: String,
}

impl RepoIdentity {
    pub(super) fn matches_record(&self, record: &FanoutRecord) -> bool {
        self.dev == record.repo_dev && self.ino == record.repo_ino
    }
}

pub(super) enum PreparedMergeOutcome {
    Merged,
    Aborted { merge_error: String },
}

pub(super) fn canonicalize_repository(repo: &Path) -> Result<PathBuf, String> {
    fs::canonicalize(repo)
        .map_err(|error| format!("resolve fanout repository {}: {error}", repo.display()))
}

pub(super) fn repo_identity(repo: &Path) -> Result<RepoIdentity, String> {
    let common = run_git(
        repo,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    let common = fs::canonicalize(&common)
        .map_err(|error| format!("resolve git common dir {common}: {error}"))?;
    let metadata = fs::metadata(&common)
        .map_err(|error| format!("stat git common dir {}: {error}", common.display()))?;
    Ok(RepoIdentity {
        common_dir: common.to_string_lossy().into_owned(),
        dev: metadata.dev().to_string(),
        ino: metadata.ino().to_string(),
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

pub(super) fn validate_sha(value: &str) -> Result<(), String> {
    if value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err("git object id is not a full hexadecimal SHA-1".to_string())
    }
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

fn run_git(cwd: &Path, args: &[&str]) -> Result<String, String> {
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
    String::from_utf8(output.stdout)
        .map(|output| output.trim().to_string())
        .map_err(|_| "git output was not UTF-8".to_string())
}

fn git_tracks_worktree(repo: &Path, worktree: &Path) -> Result<bool, String> {
    let expected = worktree.to_string_lossy();
    Ok(run_git(repo, &["worktree", "list", "--porcelain"])?
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .any(|registered| registered == expected))
}
