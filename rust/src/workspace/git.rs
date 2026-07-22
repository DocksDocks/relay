use super::authority;
use super::schema::{
    self, GitOid, ObjectFormat, PathClaimRequestV1, PreserveRequestV1, RepositoryIdentityV1,
    SourceSnapshotV1, WipPayloadV1, WipReceiptV1, WorktreeIdentityV1,
};
use crate::sha256;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

#[derive(Debug)]
pub struct OpenedRepository {
    pub root: PathBuf,
    pub identity: RepositoryIdentityV1,
    common_dir: File,
    private_dir: File,
    private_path: PathBuf,
    root_dir: File,
}

impl OpenedRepository {
    pub fn open(path: &Path) -> Result<Self, String> {
        let root_text = run_git_text(
            path,
            &["rev-parse", "--path-format=absolute", "--show-toplevel"],
        )?;
        let root = fs::canonicalize(&root_text)
            .map_err(|e| format!("canonicalize repository root {root_text}: {e}"))?;
        if root.to_str() != Some(root_text.as_str()) {
            return Err("repository root is not canonical UTF-8 absolute form".into());
        }
        let common_text = run_git_text(
            &root,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?;
        let common = fs::canonicalize(&common_text)
            .map_err(|e| format!("canonicalize Git common dir {common_text}: {e}"))?;
        if common.to_str() != Some(common_text.as_str()) {
            return Err("Git common dir is not canonical UTF-8 absolute form".into());
        }
        let private_text =
            run_git_text(&root, &["rev-parse", "--path-format=absolute", "--git-dir"])?;
        let private_path = fs::canonicalize(&private_text)
            .map_err(|e| format!("canonicalize private Git dir {private_text}: {e}"))?;
        if private_path.to_str() != Some(private_text.as_str()) {
            return Err("private Git dir is not canonical UTF-8 absolute form".into());
        }
        let open_directory = |directory: &Path, label: &str| {
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_DIRECTORY)
                .open(directory)
                .map_err(|e| format!("securely open {label} {}: {e}", directory.display()))?;
            let metadata = file.metadata().map_err(|e| format!("fstat {label}: {e}"))?;
            if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
                return Err(format!("{label} is not an EUID-owned real directory"));
            }
            Ok((file, metadata))
        };
        let (root_dir, _) = open_directory(&root, "repository root")?;
        let (common_dir, metadata) = open_directory(&common, "Git common dir")?;
        let (private_dir, _) = open_directory(&private_path, "private Git dir")?;
        let euid = unsafe { libc::geteuid() };
        let object_format = ObjectFormat::parse(&run_git_text(
            &root,
            &["rev-parse", "--show-object-format"],
        )?)?;
        let repository_id = authority::repository_id(euid, metadata.dev(), metadata.ino());
        let identity = RepositoryIdentityV1 {
            repository_id,
            common_dir_realpath: common.to_string_lossy().into_owned(),
            common_dir_dev: metadata.dev().to_string(),
            common_dir_ino: metadata.ino().to_string(),
            common_dir_owner_euid: metadata.uid().to_string(),
            euid: euid.to_string(),
            object_format,
        };
        Ok(Self {
            root,
            identity,
            common_dir,
            private_dir,
            private_path,
            root_dir,
        })
    }

    fn command(&self, args: &[&str]) -> Command {
        let root_fd = self.root_dir.as_raw_fd();
        let private_fd = self.private_dir.as_raw_fd();
        let common_fd = self.common_dir.as_raw_fd();
        let mut command = Command::new("git");
        for name in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_COMMON_DIR",
            "GIT_NAMESPACE",
            "GIT_CEILING_DIRECTORIES",
            "GIT_DISCOVERY_ACROSS_FILESYSTEM",
            "GIT_PREFIX",
        ] {
            command.env_remove(name);
        }
        command
            .args(args)
            .current_dir(format!("/proc/self/fd/{root_fd}"))
            .stdin(Stdio::null())
            .env("GIT_WORK_TREE", format!("/proc/self/fd/{root_fd}"))
            .env("GIT_DIR", format!("/proc/self/fd/{private_fd}"))
            .env("GIT_COMMON_DIR", format!("/proc/self/fd/{common_fd}"));
        unsafe {
            command.pre_exec(move || {
                for fd in [root_fd, private_fd, common_fd] {
                    let flags = libc::fcntl(fd, libc::F_GETFD);
                    if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        command
    }

    pub fn run_git_output(&self, args: &[&str]) -> Result<Output, String> {
        self.command(args).output().map_err(|e| {
            format!(
                "run bound git {} in {}: {e}",
                args.join(" "),
                self.root.display()
            )
        })
    }

    pub fn run_git_output_with_env(
        &self,
        args: &[&str],
        environment: &[(&str, &str)],
    ) -> Result<Output, String> {
        self.command(args)
            .envs(environment.iter().copied())
            .output()
            .map_err(|e| {
                format!(
                    "run bound git {} in {}: {e}",
                    args.join(" "),
                    self.root.display()
                )
            })
    }

    pub fn run_git_text(&self, args: &[&str]) -> Result<String, String> {
        let output = self.run_git_output(args)?;
        if !output.status.success() {
            return Err(format!(
                "bound git {} failed in {}: {}",
                args.join(" "),
                self.root.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        String::from_utf8(output.stdout)
            .map(|text| text.trim_end_matches(['\r', '\n']).to_string())
            .map_err(|_| "bound Git output was not UTF-8".into())
    }

    pub fn run_git_bytes(&self, args: &[&str]) -> Result<Vec<u8>, String> {
        let output = self.run_git_output(args)?;
        if !output.status.success() {
            return Err(format!(
                "bound git {} failed in {}: {}",
                args.join(" "),
                self.root.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(output.stdout)
    }

    pub fn run_git_output_with_index(&self, args: &[&str], index: &Path) -> Result<Output, String> {
        self.command(args)
            .env("GIT_INDEX_FILE", index)
            .output()
            .map_err(|e| format!("run bound Git index command {}: {e}", args.join(" ")))
    }

    pub fn worktree_identity(&self, branch_ref: &str) -> Result<WorktreeIdentityV1, String> {
        let root_meta = self
            .root_dir
            .metadata()
            .map_err(|e| format!("fstat workspace root: {e}"))?;
        let private_meta = self
            .private_dir
            .metadata()
            .map_err(|e| format!("fstat private Git dir: {e}"))?;
        let identity_sha256 = sha256::hex_digest(
            format!(
                "session-relay/worktree/v1\\0{}\\0{}\\0{}\\0{}\\0{}\\0{}",
                self.root.display(),
                root_meta.dev(),
                root_meta.ino(),
                self.private_path.display(),
                private_meta.dev(),
                private_meta.ino()
            )
            .as_bytes(),
        );
        Ok(WorktreeIdentityV1 {
            identity_sha256,
            root_realpath: self.root.to_string_lossy().into_owned(),
            root_dev: root_meta.dev().to_string(),
            root_ino: root_meta.ino().to_string(),
            root_owner_euid: root_meta.uid().to_string(),
            private_git_dir_realpath: self.private_path.to_string_lossy().into_owned(),
            private_git_dir_dev: private_meta.dev().to_string(),
            private_git_dir_ino: private_meta.ino().to_string(),
            branch_ref: branch_ref.into(),
        })
    }

    pub fn private_git_path(&self, name: &str) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}", self.private_dir.as_raw_fd())).join(name)
    }

    pub fn validate_unchanged(&self) -> Result<(), String> {
        let reopened = Self::open(&self.root)?;
        if reopened.identity != self.identity {
            return Err("repository identity or object format changed".into());
        }
        Ok(())
    }
    pub fn common_dir_fd(&self) -> i32 {
        self.common_dir.as_raw_fd()
    }
    pub fn head(&self) -> Result<GitOid, String> {
        GitOid::parse(
            &self.run_git_text(&["rev-parse", "--verify", "HEAD"])?,
            self.identity.object_format,
        )
    }
    pub fn validate_oid(&self, value: &str) -> Result<GitOid, String> {
        GitOid::parse(value, self.identity.object_format)
    }
}

pub fn run_git(cwd: &Path, args: &[&str]) -> Result<Output, String> {
    Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("run git {} in {}: {e}", args.join(" "), cwd.display()))
}
pub fn run_git_text(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let output = run_git(cwd, args)?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed in {}: {}",
            args.join(" "),
            cwd.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let text =
        String::from_utf8(output.stdout).map_err(|_| "Git output was not UTF-8".to_string())?;
    Ok(text.trim_end_matches(['\r', '\n']).to_string())
}
pub fn run_git_bytes(cwd: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let output = run_git(cwd, args)?;
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

pub fn actual_private_git_dir(worktree: &Path) -> Result<PathBuf, String> {
    let value = run_git_text(
        worktree,
        &["rev-parse", "--path-format=absolute", "--git-dir"],
    )?;
    let path = fs::canonicalize(&value)
        .map_err(|e| format!("canonicalize private Git dir {value}: {e}"))?;
    if path.to_str() != Some(value.as_str()) {
        return Err("private Git dir is not canonical UTF-8 absolute form".into());
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(&path)
        .map_err(|e| format!("securely open private Git dir: {e}"))?;
    let metadata = file
        .metadata()
        .map_err(|e| format!("fstat private Git dir: {e}"))?;
    if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err("private Git dir is not an EUID-owned real directory".into());
    }
    Ok(path)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreserveResult {
    pub receipt: WipReceiptV1,
    pub receipt_file: PathBuf,
    pub receipt_sha256: String,
}

pub fn source_snapshot(repository: &OpenedRepository) -> Result<SourceSnapshotV1, String> {
    let head = repository.head()?.as_str().to_string();
    let index_path = run_git_text(
        &repository.root,
        &["rev-parse", "--path-format=absolute", "--git-path", "index"],
    )?;
    let index_sha256 = match fs::read(&index_path) {
        Ok(bytes) => Some(sha256::hex_digest(&bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("read Git index {index_path}: {error}")),
    };
    let status = run_git_bytes(
        &repository.root,
        &["status", "--porcelain=v2", "-z", "--untracked-files=all"],
    )?;
    let inventory = run_git_bytes(
        &repository.root,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
    )?;
    Ok(SourceSnapshotV1 {
        head_oid: head,
        index_sha256,
        status_sha256: sha256::hex_digest(&status),
        tracked_untracked_inventory_sha256: sha256::hex_digest(&inventory),
    })
}

pub fn preserve(
    repository: &OpenedRepository,
    request: &PreserveRequestV1,
    request_sha256: &str,
    preserve_root: &Path,
) -> Result<PreserveResult, String> {
    repository.validate_unchanged()?;
    let base = repository.validate_oid(&request.base_commit)?;
    if base.as_str() != request.base_commit {
        return Err("preserve base OID differs from canonical request".into());
    }
    if repository.root.to_str() != Some(request.repository_path.as_str()) {
        return Err("preserve request repository path differs from opened root".into());
    }
    let ancestor = run_git(
        &repository.root,
        &["merge-base", "--is-ancestor", &request.base_commit, "HEAD"],
    )?;
    if !ancestor.status.success() {
        return Err("preserve base is not an ancestor of source HEAD".into());
    }
    authority::ensure_private_directory(preserve_root, unsafe { libc::geteuid() })?;
    let receipt_id = crate::store::uuid_v4();
    let output = preserve_root.join(&request.request_id);
    if output.exists() {
        return Err("preserve request was already materialized; refusing replacement".into());
    }
    fs::create_dir(&output)
        .map_err(|e| format!("create preserve output {}: {e}", output.display()))?;
    fs::set_permissions(&output, fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("chmod preserve output: {e}"))?;
    let before = source_snapshot(repository)?;
    let payload = match request.mode.as_str() {
        "commit" => preserve_commit(repository, request, &output)?,
        "artifact" => preserve_artifact(repository, &output)?,
        _ => return Err("preserve mode must be commit|artifact".into()),
    };
    let after = source_snapshot(repository)?;
    if before != after {
        return Err("preservation changed source HEAD/index/status/inventory".into());
    }
    let receipt = WipReceiptV1 {
        receipt_id,
        request_sha256: request_sha256.into(),
        repository: repository.identity.clone(),
        source_root: repository.root.to_string_lossy().into_owned(),
        base_commit: request.base_commit.clone(),
        mode: request.mode.clone(),
        before,
        after,
        payload,
        created_at: request.created_at.clone(),
    };
    let receipt_file = output.join("wip-receipt-v1.json");
    authority::atomic_create_jcs(&receipt_file, &receipt, 0o600)?;
    let receipt_sha256 = schema::jcs_sha256(&receipt);
    Ok(PreserveResult {
        receipt,
        receipt_file,
        receipt_sha256,
    })
}

fn preserve_commit(
    repository: &OpenedRepository,
    request: &PreserveRequestV1,
    output: &Path,
) -> Result<WipPayloadV1, String> {
    let index = output.join("temporary-index");
    let index_text = index
        .to_str()
        .ok_or_else(|| "temporary index path is not UTF-8".to_string())?;
    git_env(
        &repository.root,
        &["read-tree", &request.base_commit],
        &[("GIT_INDEX_FILE", index_text)],
    )?;
    let tracked_wip = run_git_bytes(
        &repository.root,
        &["diff", "--binary", "--full-index", "HEAD", "--"],
    )?;
    if !tracked_wip.is_empty() {
        let mut apply = Command::new("git");
        apply
            .args(["apply", "--cached", "--binary", "--whitespace=nowarn", "-"])
            .env("GIT_INDEX_FILE", index_text)
            .current_dir(&repository.root);
        let applied = command_with_input(apply, &tracked_wip, "commit-mode WIP application")?;
        if !applied.status.success() {
            return Err(format!(
                "apply commit-mode tracked WIP to requested base failed: {}",
                String::from_utf8_lossy(&applied.stderr).trim()
            ));
        }
    }
    let untracked = run_git_bytes(
        &repository.root,
        &["ls-files", "-z", "--others", "--exclude-standard"],
    )?;
    let mut untracked_paths = Vec::new();
    for bytes in untracked
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        let path = std::str::from_utf8(bytes)
            .map_err(|_| "commit-mode untracked path is not UTF-8".to_string())?;
        schema::RelPath::parse(path)?;
        untracked_paths.push(path.to_string());
    }
    if !untracked_paths.is_empty() {
        let mut args = vec!["add", "-f", "--"];
        args.extend(untracked_paths.iter().map(String::as_str));
        git_env(&repository.root, &args, &[("GIT_INDEX_FILE", index_text)])?;
    }
    let tree = git_env(
        &repository.root,
        &["write-tree"],
        &[("GIT_INDEX_FILE", index_text)],
    )?;
    repository.validate_oid(&tree)?;
    let commit = fixed_commit_tree(
        repository,
        &tree,
        &request.base_commit,
        &request.created_at,
        "session-relay preserved WIP",
    )?;
    let preserve_ref = format!("refs/docks/preserve/{}", request.request_id);
    git_env(
        &repository.root,
        &["update-ref", "--no-deref", &preserve_ref, &commit, ""],
        &[],
    )?;
    let published = run_git_text(&repository.root, &["rev-parse", "--verify", &preserve_ref])?;
    if published != commit {
        return Err("preserve ref read-back differs from created commit".into());
    }
    let published_tree = run_git_text(
        &repository.root,
        &["rev-parse", "--verify", &format!("{commit}^{{tree}}")],
    )?;
    if published_tree != tree {
        return Err("preserved commit tree read-back differs".into());
    }
    let published_parent = run_git_text(
        &repository.root,
        &["rev-parse", "--verify", &format!("{commit}^")],
    )?;
    if published_parent != request.base_commit {
        return Err("preserved commit parent read-back differs".into());
    }
    fs::set_permissions(&index, fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("chmod temporary index: {e}"))?;
    let index_bytes = fs::read(&index).map_err(|e| format!("read temporary index: {e}"))?;
    let temporary_index_sha256 = sha256::hex_digest(&index_bytes);
    if fs::read(&index).map_err(|e| format!("read back temporary index: {e}"))? != index_bytes {
        return Err("temporary index read-back differs".into());
    }
    Ok(WipPayloadV1::Commit {
        temporary_index_sha256,
        tree_oid: tree,
        preserved_commit: commit,
        preserve_ref,
    })
}

#[derive(Debug)]
enum ArtifactMember {
    Regular {
        path: String,
        file: File,
        stat: rustix::fs::Stat,
        digest: String,
        mode: u32,
    },
    Symlink {
        path: String,
        target: String,
        mode: u32,
    },
    Unsupported {
        path: String,
    },
}

impl ArtifactMember {
    fn path(&self) -> &str {
        match self {
            Self::Regular { path, .. }
            | Self::Symlink { path, .. }
            | Self::Unsupported { path } => path,
        }
    }
}

fn same_artifact_stat(left: &rustix::fs::Stat, right: &rustix::fs::Stat) -> bool {
    left.st_dev == right.st_dev
        && left.st_ino == right.st_ino
        && left.st_mode == right.st_mode
        && left.st_nlink == right.st_nlink
        && left.st_size == right.st_size
}

fn read_stable_artifact_file(
    path: &str,
    file: &mut File,
    expected: &rustix::fs::Stat,
) -> Result<Vec<u8>, String> {
    let before =
        rustix::fs::fstat(&*file).map_err(|e| format!("fstat artifact member {path}: {e}"))?;
    if !same_artifact_stat(&before, expected) {
        return Err(format!("artifact member {path} identity/type changed"));
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|e| format!("rewind artifact member {path}: {e}"))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|e| format!("read artifact member {path}: {e}"))?;
    let after = rustix::fs::fstat(&*file)
        .map_err(|e| format!("fstat artifact member {path} after read: {e}"))?;
    if !same_artifact_stat(&before, &after) {
        return Err(format!("artifact member {path} changed while it was read"));
    }
    Ok(bytes)
}

fn tracked_artifact_modes(
    repository: &OpenedRepository,
) -> Result<std::collections::HashMap<String, String>, String> {
    let bytes = repository.run_git_bytes(&["ls-files", "--stage", "-z"])?;
    let mut tracked = std::collections::HashMap::new();
    for record in bytes
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        let tab = record
            .iter()
            .position(|byte| *byte == b'\t')
            .ok_or_else(|| "tracked Git inventory record has no path".to_string())?;
        let header = std::str::from_utf8(&record[..tab])
            .map_err(|_| "tracked Git inventory header is not UTF-8".to_string())?;
        let mode = header
            .split_whitespace()
            .next()
            .ok_or_else(|| "tracked Git inventory record has no mode".to_string())?;
        let path = std::str::from_utf8(&record[tab + 1..])
            .map_err(|_| "tracked Git path is not UTF-8".to_string())?;
        schema::RelPath::parse(path)?;
        if let Some(previous) = tracked.insert(path.to_string(), mode.to_string()) {
            if previous != mode {
                return Err(format!(
                    "tracked Git path {path} has conflicting staged types"
                ));
            }
        }
    }
    Ok(tracked)
}

fn ignored_artifact_paths(
    repository: &OpenedRepository,
    members: &[ArtifactMember],
) -> Result<std::collections::HashSet<String>, String> {
    if members.is_empty() {
        return Ok(std::collections::HashSet::new());
    }
    let mut input = Vec::new();
    for member in members {
        input.extend_from_slice(member.path().as_bytes());
        input.push(0);
    }
    let command = repository.command(&["check-ignore", "--stdin", "-z"]);
    let output = command_with_input(command, &input, "Git artifact ignore classification")?;
    if !output.status.success() && output.status.code() != Some(1) {
        return Err(format!(
            "Git artifact ignore classification failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let candidates = members
        .iter()
        .map(ArtifactMember::path)
        .collect::<std::collections::HashSet<_>>();
    let mut ignored = std::collections::HashSet::new();
    for value in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|value| !value.is_empty())
    {
        let path =
            std::str::from_utf8(value).map_err(|_| "ignored Git path is not UTF-8".to_string())?;
        if !candidates.contains(path) {
            return Err("Git ignore classification returned an unrequested path".into());
        }
        ignored.insert(path.to_string());
    }
    Ok(ignored)
}

fn safe_artifact_symlink(path: &str, target: &str) -> Result<(), String> {
    let target = Path::new(target);
    if target.is_absolute() {
        return Err(format!("artifact symlink {path} escapes the source root"));
    }
    let mut resolved = Path::new(path)
        .parent()
        .into_iter()
        .flat_map(Path::components)
        .filter_map(|component| match component {
            std::path::Component::Normal(value) => Some(value.to_os_string()),
            _ => None,
        })
        .collect::<Vec<_>>();
    for component in target.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::Normal(value) => resolved.push(value.to_os_string()),
            std::path::Component::ParentDir => {
                if resolved.pop().is_none() {
                    return Err(format!("artifact symlink {path} escapes the source root"));
                }
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                return Err(format!("artifact symlink {path} escapes the source root"));
            }
        }
    }
    if resolved.first().and_then(|value| value.to_str()) == Some(".git") {
        return Err(format!(
            "artifact symlink {path} targets repository metadata"
        ));
    }
    Ok(())
}

fn portable_permission_mode(mode: impl Into<u32>) -> u32 {
    mode.into() & 0o7777
}

fn walk_artifact_source(
    directory: &File,
    relative: &Path,
    tracked: &std::collections::HashMap<String, String>,
    members: &mut Vec<ArtifactMember>,
) -> Result<(), String> {
    use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags, openat, statat};
    let mut names = Dir::read_from(directory)
        .map_err(|e| {
            format!(
                "securely enumerate artifact source {}: {e}",
                relative.display()
            )
        })?
        .map(|entry| entry.map(|entry| entry.file_name().to_owned()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read artifact source entry in {}: {e}", relative.display()))?;
    names.sort_by(|left, right| left.to_bytes().cmp(right.to_bytes()));
    for name in names {
        if name.to_bytes() == b"." || name.to_bytes() == b".." {
            continue;
        }
        let name_text = name
            .to_str()
            .map_err(|_| "artifact source path is not UTF-8".to_string())?;
        let child_relative = relative.join(name_text);
        let path = child_relative
            .to_str()
            .ok_or_else(|| "artifact source path is not UTF-8".to_string())?
            .to_string();
        schema::RelPath::parse(&path)?;
        if path == ".git" {
            continue;
        }
        let observed = statat(directory, &name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|e| format!("inspect artifact source {path}: {e}"))?;
        let file_type = FileType::from_raw_mode(observed.st_mode);
        if file_type.is_dir() {
            let child = File::from(
                openat(
                    directory,
                    &name,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(|e| format!("securely open artifact directory {path}: {e}"))?,
            );
            let opened = rustix::fs::fstat(&child)
                .map_err(|e| format!("fstat artifact directory {path}: {e}"))?;
            if !same_artifact_stat(&observed, &opened) {
                return Err(format!("artifact directory {path} identity/type changed"));
            }
            if let Some(mode) = tracked.get(&path) {
                if mode == "160000" {
                    continue;
                }
                return Err(format!(
                    "tracked artifact path {path} changed type to a directory"
                ));
            }
            walk_artifact_source(&child, &child_relative, tracked, members)?;
        } else if file_type.is_file() {
            if tracked.contains_key(&path) {
                continue;
            }
            let mut file = File::from(
                openat(
                    directory,
                    &name,
                    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(|e| format!("securely open artifact member {path}: {e}"))?,
            );
            let opened = rustix::fs::fstat(&file)
                .map_err(|e| format!("fstat artifact member {path}: {e}"))?;
            if !same_artifact_stat(&observed, &opened) {
                return Err(format!("artifact member {path} identity/type changed"));
            }
            let bytes = read_stable_artifact_file(&path, &mut file, &opened)?;
            members.push(ArtifactMember::Regular {
                path,
                digest: sha256::hex_digest(&bytes),
                mode: portable_permission_mode(opened.st_mode),
                file,
                stat: opened,
            });
        } else if file_type.is_symlink() {
            if tracked.contains_key(&path) {
                continue;
            }
            let target = rustix::fs::readlinkat(directory, &name, Vec::new())
                .map_err(|e| format!("read artifact symlink {path}: {e}"))?;
            let after = statat(directory, &name, AtFlags::SYMLINK_NOFOLLOW)
                .map_err(|e| format!("reinspect artifact symlink {path}: {e}"))?;
            if !same_artifact_stat(&observed, &after) {
                return Err(format!("artifact symlink {path} identity/type changed"));
            }
            let target = target
                .to_str()
                .map_err(|_| format!("artifact symlink {path} target is not UTF-8"))?
                .to_string();
            members.push(ArtifactMember::Symlink {
                path,
                target,
                mode: portable_permission_mode(observed.st_mode),
            });
        } else {
            members.push(ArtifactMember::Unsupported { path });
        }
    }
    Ok(())
}

fn secure_artifact_inventory(
    repository: &OpenedRepository,
    git_inventory: &[u8],
) -> Result<(Vec<String>, Vec<ArtifactMember>), String> {
    let tracked = tracked_artifact_modes(repository)?;
    let root = repository
        .root_dir
        .try_clone()
        .map_err(|e| format!("duplicate repository root descriptor: {e}"))?;
    let mut members = Vec::new();
    walk_artifact_source(&root, Path::new(""), &tracked, &mut members)?;
    let ignored = ignored_artifact_paths(repository, &members)?;
    members.retain(|member| !ignored.contains(member.path()));
    for member in &mut members {
        match member {
            ArtifactMember::Regular {
                path,
                file,
                stat,
                digest,
                ..
            } => {
                if stat.st_nlink != 1 {
                    return Err(format!(
                        "artifact member {path} is a hard-linked regular file"
                    ));
                }
                let bytes = read_stable_artifact_file(path, file, stat)?;
                if sha256::hex_digest(&bytes) != *digest {
                    return Err(format!("artifact member {path} content hash changed"));
                }
            }
            ArtifactMember::Symlink { path, target, .. } => safe_artifact_symlink(path, target)?,
            ArtifactMember::Unsupported { path } => {
                return Err(format!("artifact member {path} has an unsupported type"));
            }
        }
    }
    members.sort_by(|left, right| left.path().as_bytes().cmp(right.path().as_bytes()));
    if members
        .windows(2)
        .any(|pair| pair[0].path() == pair[1].path())
    {
        return Err("artifact inventory contains duplicate paths".into());
    }
    let entries = members
        .iter()
        .map(|member| member.path().to_string())
        .collect::<Vec<_>>();
    let mut inventory = Vec::new();
    for entry in &entries {
        inventory.extend_from_slice(entry.as_bytes());
        inventory.push(0);
    }
    if inventory != git_inventory {
        return Err("secure artifact inventory differs from Git untracked inventory".into());
    }
    Ok((entries, members))
}

fn preserve_artifact(repository: &OpenedRepository, output: &Path) -> Result<WipPayloadV1, String> {
    let binary_diff_staging = output.join("tracked-full-index.binary");
    let diff = repository.run_git_bytes(&["diff", "--binary", "--full-index", "HEAD", "--"])?;
    write_private(&binary_diff_staging, &diff)?;
    if fs::read(&binary_diff_staging).map_err(|e| format!("read back tracked artifact: {e}"))?
        != diff
    {
        return Err("tracked artifact read-back differs".into());
    }
    let inventory_bytes =
        repository.run_git_bytes(&["ls-files", "-z", "--others", "--exclude-standard"])?;
    let (entries, mut members) = secure_artifact_inventory(repository, &inventory_bytes)?;
    let mut inventory = Vec::new();
    for entry in &entries {
        inventory.extend_from_slice(entry.as_bytes());
        inventory.push(0)
    }
    let untracked_inventory_staging = output.join("untracked.inventory");
    write_private(&untracked_inventory_staging, &inventory)?;
    if fs::read(&untracked_inventory_staging)
        .map_err(|e| format!("read back artifact inventory: {e}"))?
        != inventory
    {
        return Err("artifact inventory read-back differs".into());
    }
    let untracked_archive_staging = output.join("untracked.pax");
    create_pax_archive(&untracked_archive_staging, &mut members)?;
    let archive_file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&untracked_archive_staging)
        .map_err(|e| format!("reopen PAX archive: {e}"))?;
    archive_file
        .sync_all()
        .map_err(|e| format!("sync PAX archive: {e}"))?;
    let metadata = archive_file
        .metadata()
        .map_err(|e| format!("inspect PAX archive: {e}"))?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.nlink() != 1
        || metadata.mode() & 0o777 != 0o600
    {
        return Err("PAX archive is not an EUID-owned single-link mode-0600 regular file".into());
    }
    let archive =
        fs::read(&untracked_archive_staging).map_err(|e| format!("read PAX archive: {e}"))?;
    let binary_diff =
        publish_content_addressed(&binary_diff_staging, &diff, "tracked-full-index.binary")?;
    let untracked_inventory = publish_content_addressed(
        &untracked_inventory_staging,
        &inventory,
        "untracked.inventory",
    )?;
    let untracked_archive =
        publish_content_addressed(&untracked_archive_staging, &archive, "untracked.pax")?;
    Ok(WipPayloadV1::Artifact {
        binary_diff: binary_diff.to_string_lossy().into_owned(),
        untracked_inventory: untracked_inventory.to_string_lossy().into_owned(),
        untracked_archive: untracked_archive.to_string_lossy().into_owned(),
        archive_format: "pax".into(),
        entries,
    })
}

fn pax_record(key: &str, value: &str) -> Vec<u8> {
    let body_len = key.len() + value.len() + 2;
    let mut digits = 1;
    loop {
        let length = digits + 1 + body_len;
        let next_digits = length.to_string().len();
        if next_digits == digits {
            return format!("{length} {key}={value}\n").into_bytes();
        }
        digits = next_digits;
    }
}

fn write_tar_octal(field: &mut [u8], value: u64, label: &str) -> Result<(), String> {
    let encoded = format!("{value:o}");
    if encoded.len() + 1 > field.len() {
        return Err(format!("artifact {label} is too large for a PAX header"));
    }
    field.fill(b'0');
    let start = field.len() - 1 - encoded.len();
    field[start..start + encoded.len()].copy_from_slice(encoded.as_bytes());
    field[field.len() - 1] = 0;
    Ok(())
}

fn tar_header(name: &str, mode: u32, size: u64, entry_type: u8) -> Result<[u8; 512], String> {
    if name.len() > 100 {
        return Err("internal PAX member name exceeds the USTAR field".into());
    }
    let mut header = [0_u8; 512];
    header[..name.len()].copy_from_slice(name.as_bytes());
    write_tar_octal(&mut header[100..108], u64::from(mode & 0o7777), "mode")?;
    write_tar_octal(&mut header[108..116], 0, "uid")?;
    write_tar_octal(&mut header[116..124], 0, "gid")?;
    write_tar_octal(&mut header[124..136], size, "member size")?;
    write_tar_octal(&mut header[136..148], 0, "mtime")?;
    header[148..156].fill(b' ');
    header[156] = entry_type;
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    let checksum = header.iter().map(|byte| u64::from(*byte)).sum::<u64>();
    let encoded = format!("{checksum:06o}");
    if encoded.len() != 6 {
        return Err("PAX header checksum overflow".into());
    }
    header[148..154].copy_from_slice(encoded.as_bytes());
    header[154] = 0;
    header[155] = b' ';
    Ok(header)
}

fn write_tar_payload(archive: &mut File, bytes: &[u8]) -> Result<(), String> {
    archive
        .write_all(bytes)
        .map_err(|e| format!("write PAX payload: {e}"))?;
    let padding = (512 - bytes.len() % 512) % 512;
    if padding != 0 {
        archive
            .write_all(&[0_u8; 512][..padding])
            .map_err(|e| format!("pad PAX payload: {e}"))?;
    }
    Ok(())
}

fn create_pax_archive(archive: &Path, members: &mut [ArtifactMember]) -> Result<(), String> {
    #[cfg(test)]
    run_before_pax_archive();
    let mut archive = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(archive)
        .map_err(|e| format!("create PAX archive: {e}"))?;
    for (index, member) in members.iter_mut().enumerate() {
        let path = member.path().to_string();
        let mut pax = pax_record("path", &path);
        if let ArtifactMember::Symlink { target, .. } = member {
            pax.extend_from_slice(&pax_record("linkpath", target));
        }
        let extended_name = format!("PaxHeaders/{index}");
        let extended = tar_header(
            &extended_name,
            0o600,
            pax.len()
                .try_into()
                .map_err(|_| "PAX extended header is too large".to_string())?,
            b'x',
        )?;
        archive
            .write_all(&extended)
            .map_err(|e| format!("write PAX extended header for {path}: {e}"))?;
        write_tar_payload(&mut archive, &pax)?;
        let member_name = format!("PaxMember/{index}");
        match member {
            ArtifactMember::Regular {
                file,
                stat,
                digest,
                mode,
                ..
            } => {
                let bytes = read_stable_artifact_file(&path, file, stat)?;
                if sha256::hex_digest(&bytes) != *digest {
                    return Err(format!(
                        "artifact member {path} content hash changed at archive creation"
                    ));
                }
                let header = tar_header(
                    &member_name,
                    *mode,
                    bytes
                        .len()
                        .try_into()
                        .map_err(|_| format!("artifact member {path} is too large"))?,
                    b'0',
                )?;
                archive
                    .write_all(&header)
                    .map_err(|e| format!("write PAX header for {path}: {e}"))?;
                write_tar_payload(&mut archive, &bytes)?;
            }
            ArtifactMember::Symlink { mode, .. } => {
                let header = tar_header(&member_name, *mode, 0, b'2')?;
                archive
                    .write_all(&header)
                    .map_err(|e| format!("write PAX symlink header for {path}: {e}"))?;
            }
            ArtifactMember::Unsupported { .. } => {
                return Err(format!("artifact member {path} has an unsupported type"));
            }
        }
    }
    archive
        .write_all(&[0_u8; 1024])
        .and_then(|_| archive.sync_all())
        .map_err(|e| format!("persist PAX archive: {e}"))
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| format!("create {}: {e}", path.display()))?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|e| format!("persist {}: {e}", path.display()))
}

fn publish_content_addressed(staging: &Path, bytes: &[u8], label: &str) -> Result<PathBuf, String> {
    let digest = sha256::hex_digest(bytes);
    let target = staging
        .parent()
        .ok_or_else(|| "artifact staging path has no parent".to_string())?
        .join(format!("{digest}.{label}"));
    fs::rename(staging, &target).map_err(|e| {
        format!(
            "publish content-addressed artifact {}: {e}",
            target.display()
        )
    })?;
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(target.parent().unwrap())
        .map_err(|e| format!("open artifact directory for fsync: {e}"))?;
    directory
        .sync_all()
        .map_err(|e| format!("fsync artifact directory: {e}"))?;
    Ok(target)
}

fn read_content_addressed(path: &Path, label: &str) -> Result<Vec<u8>, String> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("content-addressed {label} path is not UTF-8"))?;
    let digest = file_name
        .strip_suffix(&format!(".{label}"))
        .ok_or_else(|| format!("{label} path is not content-addressed"))?;
    schema::Sha256Digest::parse(digest)?;
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| format!("securely open {label}: {e}"))?;
    let metadata = file.metadata().map_err(|e| format!("fstat {label}: {e}"))?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.nlink() != 1
        || metadata.mode() & 0o777 != 0o600
    {
        return Err(format!(
            "{label} is not an EUID-owned single-link mode-0600 regular file"
        ));
    }
    let mut bytes = Vec::new();
    use std::io::Read;
    file.read_to_end(&mut bytes)
        .map_err(|e| format!("read {label}: {e}"))?;
    if sha256::hex_digest(&bytes) != digest {
        return Err(format!(
            "{label} content digest differs from its receipt-bound name"
        ));
    }
    Ok(bytes)
}
#[cfg(test)]
thread_local! {
    static AFTER_ARTIFACT_VERIFY: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
    static BEFORE_PAX_ARCHIVE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
}

#[cfg(test)]
fn set_before_pax_archive(hook: impl FnOnce() + 'static) {
    BEFORE_PAX_ARCHIVE.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
}

#[cfg(test)]
fn run_before_pax_archive() {
    BEFORE_PAX_ARCHIVE.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook();
        }
    });
}

#[cfg(test)]
fn set_after_artifact_verify(hook: impl FnOnce() + 'static) {
    AFTER_ARTIFACT_VERIFY.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
}

#[cfg(test)]
fn run_after_artifact_verify() {
    AFTER_ARTIFACT_VERIFY.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook();
        }
    });
}

fn command_with_input(mut command: Command, input: &[u8], label: &str) -> Result<Output, String> {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|e| format!("spawn {label}: {e}"))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| format!("{label} stdin was not piped"))?;
    let (write_result, output) = std::thread::scope(|scope| {
        let writer = scope.spawn(move || stdin.write_all(input));
        let output = child.wait_with_output();
        (writer.join(), output)
    });
    write_result
        .map_err(|_| format!("{label} input writer panicked"))?
        .map_err(|e| format!("write {label} input: {e}"))?;
    output.map_err(|e| format!("wait for {label}: {e}"))
}
fn git_env(cwd: &Path, args: &[&str], environment: &[(&str, &str)]) -> Result<String, String> {
    let output = Command::new("git")
        .args(args)
        .envs(environment.iter().copied())
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("run git {}: {e}", args.join(" ")))?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8(output.stdout)
        .map(|v| v.trim().to_string())
        .map_err(|_| "Git output was not UTF-8".into())
}
fn fixed_commit_tree(
    repository: &OpenedRepository,
    tree: &str,
    parent: &str,
    timestamp: &str,
    message: &str,
) -> Result<String, String> {
    let environment = [
        ("GIT_AUTHOR_NAME", "Session Relay"),
        ("GIT_AUTHOR_EMAIL", "session-relay@localhost"),
        ("GIT_AUTHOR_DATE", timestamp),
        ("GIT_COMMITTER_NAME", "Session Relay"),
        ("GIT_COMMITTER_EMAIL", "session-relay@localhost"),
        ("GIT_COMMITTER_DATE", timestamp),
    ];
    let output = repository.run_git_output_with_env(
        &["commit-tree", tree, "-p", parent, "-m", message],
        &environment,
    )?;
    if !output.status.success() {
        return Err(format!(
            "bound git commit-tree failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8(output.stdout)
        .map(|value| value.trim().to_string())
        .map_err(|_| "Git output was not UTF-8".into())
}

pub fn provision_worktree(
    repository: &OpenedRepository,
    root: &Path,
    branch_ref: &str,
    session_id: &str,
    task_slug: &str,
    base_commit: &str,
) -> Result<WorktreeIdentityV1, String> {
    if root.exists() {
        return Err(format!(
            "deterministic workspace root {} already exists; suffixing is forbidden",
            root.display()
        ));
    }
    let short_branch = branch_ref
        .strip_prefix("refs/heads/")
        .ok_or_else(|| "workspace branch is not under refs/heads".to_string())?;
    let reason = format!("session-relay:{session_id}");
    let root_text = root
        .to_str()
        .ok_or_else(|| "workspace root is not UTF-8".to_string())?;
    run_git_text(
        &repository.root,
        &[
            "worktree",
            "add",
            "--lock",
            "--reason",
            &reason,
            "-b",
            short_branch,
            root_text,
            base_commit,
        ],
    )?;
    let identity = worktree_identity(root, branch_ref)?;
    if run_git_text(root, &["symbolic-ref", "-q", "HEAD"])? != branch_ref {
        return Err("created workspace symbolic branch differs from deterministic ref".into());
    }
    if !branch_ref.ends_with(&format!("/{task_slug}")) {
        return Err("created workspace branch task slug differs".into());
    }
    Ok(identity)
}

pub fn worktree_identity(root: &Path, branch_ref: &str) -> Result<WorktreeIdentityV1, String> {
    let canonical =
        fs::canonicalize(root).map_err(|e| format!("canonicalize workspace root: {e}"))?;
    if canonical != root {
        return Err("workspace root is not canonical or traverses a symlink".into());
    }
    let root_meta =
        fs::symlink_metadata(&canonical).map_err(|e| format!("stat workspace root: {e}"))?;
    if !root_meta.is_dir() || root_meta.uid() != unsafe { libc::geteuid() } {
        return Err("workspace root is not an EUID-owned real directory".into());
    }
    let private = actual_private_git_dir(root)?;
    let private_meta =
        fs::symlink_metadata(&private).map_err(|e| format!("stat private Git dir: {e}"))?;
    let identity_sha256 = sha256::hex_digest(
        format!(
            "session-relay/worktree/v1\\0{}\\0{}\\0{}\\0{}\\0{}\\0{}",
            canonical.display(),
            root_meta.dev(),
            root_meta.ino(),
            private.display(),
            private_meta.dev(),
            private_meta.ino()
        )
        .as_bytes(),
    );
    Ok(WorktreeIdentityV1 {
        identity_sha256,
        root_realpath: canonical.to_string_lossy().into_owned(),
        root_dev: root_meta.dev().to_string(),
        root_ino: root_meta.ino().to_string(),
        root_owner_euid: root_meta.uid().to_string(),
        private_git_dir_realpath: private.to_string_lossy().into_owned(),
        private_git_dir_dev: private_meta.dev().to_string(),
        private_git_dir_ino: private_meta.ino().to_string(),
        branch_ref: branch_ref.into(),
    })
}

pub fn apply_wip(
    repository: &OpenedRepository,
    worktree: &Path,
    branch_ref: &str,
    base_commit: &str,
    receipt: &WipReceiptV1,
    timestamp: &str,
) -> Result<String, String> {
    if run_git_text(worktree, &["symbolic-ref", "-q", "HEAD"])? != branch_ref {
        return Err("workspace branch changed before WIP application".into());
    }
    if run_git_text(worktree, &["rev-parse", "--verify", "HEAD"])? != base_commit {
        return Err("workspace HEAD changed before WIP application".into());
    }
    if receipt.repository != repository.identity
        || receipt.source_root != repository.root.to_string_lossy()
    {
        return Err("WIP receipt repository provenance differs at application".into());
    }
    match &receipt.payload {
        WipPayloadV1::Commit {
            tree_oid,
            preserved_commit,
            preserve_ref,
            ..
        } => {
            repository.validate_oid(tree_oid)?;
            repository.validate_oid(preserved_commit)?;
            let published =
                run_git_text(&repository.root, &["rev-parse", "--verify", preserve_ref])?;
            if &published != preserved_commit {
                return Err("preserve ref changed before WIP application".into());
            }
            let preserved_tree = run_git_text(
                &repository.root,
                &[
                    "rev-parse",
                    "--verify",
                    &format!("{preserved_commit}^{{tree}}"),
                ],
            )?;
            if &preserved_tree != tree_oid {
                return Err("preserved commit tree differs before WIP application".into());
            }
            let parent = run_git_text(
                &repository.root,
                &["rev-parse", "--verify", &format!("{preserved_commit}^")],
            )?;
            if parent != base_commit {
                return Err("preserved commit parent differs before WIP application".into());
            }
            run_git_text(worktree, &["read-tree", "--reset", "-u", tree_oid])?;
        }
        WipPayloadV1::Artifact {
            binary_diff,
            untracked_inventory,
            untracked_archive,
            entries,
            ..
        } => {
            let diff = read_content_addressed(Path::new(binary_diff), "tracked-full-index.binary")?;
            let inventory =
                read_content_addressed(Path::new(untracked_inventory), "untracked.inventory")?;
            let archive = read_content_addressed(Path::new(untracked_archive), "untracked.pax")?;
            #[cfg(test)]
            run_after_artifact_verify();
            let parsed = inventory
                .split(|byte| *byte == 0)
                .filter(|value| !value.is_empty())
                .map(|value| {
                    std::str::from_utf8(value)
                        .map(str::to_string)
                        .map_err(|_| "artifact inventory is not UTF-8".to_string())
                })
                .collect::<Result<Vec<_>, _>>()?;
            if &parsed != entries {
                return Err("artifact inventory differs from receipt entries".into());
            }
            let mut list = Command::new("tar");
            list.args(["-tf", "-"]);
            let listed = command_with_input(list, &archive, "artifact archive listing")?;
            if !listed.status.success() {
                return Err(format!(
                    "artifact archive cannot be listed: {}",
                    String::from_utf8_lossy(&listed.stderr).trim()
                ));
            }
            let archive_entries = String::from_utf8(listed.stdout)
                .map_err(|_| "artifact archive member is not UTF-8".to_string())?
                .lines()
                .map(|line| line.trim_end_matches('/').to_string())
                .filter(|line| !line.is_empty())
                .collect::<Vec<_>>();
            if archive_entries != *entries {
                return Err("artifact archive membership differs from receipt".into());
            }
            if !diff.is_empty() {
                let mut apply = Command::new("git");
                apply
                    .args(["apply", "--index", "--binary", "-"])
                    .current_dir(worktree);
                let output = command_with_input(apply, &diff, "tracked artifact application")?;
                if !output.status.success() {
                    return Err(format!(
                        "apply tracked artifact failed: {}",
                        String::from_utf8_lossy(&output.stderr).trim()
                    ));
                }
            }
            let mut extract = Command::new("tar");
            extract
                .args([
                    "--extract",
                    "--no-same-owner",
                    "--no-same-permissions",
                    "--keep-old-files",
                    "-f",
                    "-",
                ])
                .current_dir(worktree);
            let output = command_with_input(extract, &archive, "artifact extraction")?;
            if !output.status.success() {
                return Err(format!(
                    "artifact extraction failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ));
            }
            if !entries.is_empty() {
                let mut args = vec!["add", "--"];
                args.extend(entries.iter().map(String::as_str));
                run_git_text(worktree, &args)?;
            }
        }
    }
    let tree = run_git_text(worktree, &["write-tree"])?;
    let commit = fixed_commit_tree(
        repository,
        &tree,
        base_commit,
        timestamp,
        "session-relay applied WIP",
    )?;
    repository.validate_oid(&commit)?;
    run_git_text(
        &repository.root,
        &["update-ref", "--no-deref", branch_ref, &commit, base_commit],
    )?;
    run_git_text(worktree, &["reset", "--hard", &commit])?;
    Ok(commit)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NameStatusChange {
    pub status: String,
    pub source: Option<String>,
    pub destination: String,
}
pub fn parse_name_status_z(bytes: &[u8]) -> Result<Vec<NameStatusChange>, String> {
    if !bytes.is_empty() && !bytes.ends_with(&[0]) {
        return Err("name-status output is truncated".into());
    }
    let fields = bytes
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
        .collect::<Vec<_>>();
    let mut index = 0;
    let mut changes = Vec::new();
    while index < fields.len() {
        let status = std::str::from_utf8(fields[index])
            .map_err(|_| "name-status token is not UTF-8".to_string())?
            .to_string();
        index += 1;
        let rename = status.starts_with('R') || status.starts_with('C');
        if !matches!(
            status.as_bytes().first(),
            Some(b'A' | b'M' | b'D' | b'T' | b'R' | b'C')
        ) {
            return Err(format!("unsupported name-status code {status}"));
        }
        let first = fields
            .get(index)
            .ok_or_else(|| "name-status path is missing".to_string())?;
        index += 1;
        let first = std::str::from_utf8(first)
            .map_err(|_| "name-status path is not UTF-8".to_string())?
            .to_string();
        schema::RelPath::parse(&first)?;
        if rename {
            let second = fields
                .get(index)
                .ok_or_else(|| "rename/copy destination is missing".to_string())?;
            index += 1;
            let second = std::str::from_utf8(second)
                .map_err(|_| "name-status destination is not UTF-8".to_string())?
                .to_string();
            schema::RelPath::parse(&second)?;
            changes.push(NameStatusChange {
                status,
                source: Some(first),
                destination: second,
            })
        } else {
            changes.push(NameStatusChange {
                status,
                source: None,
                destination: first,
            })
        }
    }
    Ok(changes)
}

pub fn validate_changed_paths(
    changes: &[NameStatusChange],
    claims: &[PathClaimRequestV1],
) -> Result<(), String> {
    let owns = |path: &str| {
        claims.iter().any(|claim| {
            path == claim.path
                || (claim.path_type == "directory"
                    && path
                        .strip_prefix(&claim.path)
                        .is_some_and(|suffix| suffix.starts_with('/')))
        })
    };
    for change in changes {
        if !owns(&change.destination)
            || change.source.as_deref().is_some_and(|source| !owns(source))
        {
            return Err(format!(
                "changed path is outside admitted claims: {:?}",
                change
            ));
        }
    }
    Ok(())
}

pub fn create_worker_commit(
    repository: &OpenedRepository,
    branch_ref: &str,
    message: &str,
    timestamp: &str,
) -> Result<String, String> {
    if message.is_empty() || message.contains('\0') {
        return Err("worker commit message is invalid".into());
    }
    if repository.run_git_text(&["symbolic-ref", "-q", "HEAD"])? != branch_ref {
        return Err("workspace branch changed before worker commit".into());
    }
    let head = repository.run_git_text(&["rev-parse", "--verify", "HEAD"])?;
    repository.validate_oid(&head)?;
    for marker in [
        "MERGE_HEAD",
        "REBASE_HEAD",
        "CHERRY_PICK_HEAD",
        "BISECT_START",
    ] {
        if repository.private_git_path(marker).exists() {
            return Err(format!(
                "worker commit refused during forbidden Git operation {marker}"
            ));
        }
    }
    let tree = repository.run_git_text(&["write-tree"])?;
    let commit = fixed_commit_tree(repository, &tree, &head, timestamp, message)?;
    repository.validate_oid(&commit)?;
    repository.run_git_text(&["update-ref", "--no-deref", branch_ref, &commit, &head])?;
    Ok(commit)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OrderedProducedCommit {
    pub oid: String,
    pub parent_oid: String,
    pub source: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OrderedIntegrationRequest {
    pub integration_root: PathBuf,
    pub integration_branch_ref: String,
    pub expected_integration_head: String,
    pub worker_root: PathBuf,
    pub worker_branch_ref: String,
    pub expected_worker_head: String,
    pub base_commit: String,
    pub produced_commits: Vec<OrderedProducedCommit>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CoordinatorIntegrationOutcome {
    Integrated,
    NeedsUserAction,
    Rejected,
}
impl CoordinatorIntegrationOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Integrated => "integrated",
            Self::NeedsUserAction => "needs_user_action",
            Self::Rejected => "rejected",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CoordinatorIntegrationResult {
    pub outcome: CoordinatorIntegrationOutcome,
    pub pre_head: String,
    pub post_head: String,
    pub output_oids: Vec<String>,
    pub conflict_paths: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PristineGitState {
    pub head_oid: String,
    pub head_tree_oid: String,
    pub index_tree_oid: String,
    pub index_file_sha256: String,
    pub index_entries_sha256: String,
    pub status_sha256: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OrderedIntegrationPrestate {
    pub integration_state: PristineGitState,
    pub integration_index_bytes: Vec<u8>,
}

#[derive(Debug)]
struct PreparedIntegration {
    integration: OpenedRepository,
    worker: OpenedRepository,
    integration_state: PristineGitState,
    worker_state: PristineGitState,
    integration_index_bytes: Vec<u8>,
    worker_index_bytes: Vec<u8>,
}

fn same_pristine_semantics(left: &PristineGitState, right: &PristineGitState) -> bool {
    left.head_oid == right.head_oid
        && left.head_tree_oid == right.head_tree_oid
        && left.index_tree_oid == right.index_tree_oid
        && left.index_entries_sha256 == right.index_entries_sha256
        && left.status_sha256 == right.status_sha256
}

const COORDINATOR_GIT_ENVIRONMENT: [&str; 10] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
    "GIT_CEILING_DIRECTORIES",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
    "GIT_PREFIX",
];

fn coordinator_git_output(cwd: &Path, args: &[&str]) -> Result<Output, String> {
    let mut command = Command::new("git");
    command.args(args).current_dir(cwd).stdin(Stdio::null());
    for name in COORDINATOR_GIT_ENVIRONMENT {
        command.env_remove(name);
    }
    command.env("GIT_OPTIONAL_LOCKS", "0");
    command.output().map_err(|e| {
        format!(
            "run coordinator git {} in {}: {e}",
            args.join(" "),
            cwd.display()
        )
    })
}

fn coordinator_git_text(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let output = coordinator_git_output(cwd, args)?;
    if !output.status.success() {
        return Err(format!(
            "coordinator git {} failed in {}: {}",
            args.join(" "),
            cwd.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let text = String::from_utf8(output.stdout)
        .map_err(|_| "coordinator Git output was not UTF-8".to_string())?;
    Ok(text.trim_end_matches(['\r', '\n']).to_string())
}

fn coordinator_git_bytes(cwd: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let output = coordinator_git_output(cwd, args)?;
    if !output.status.success() {
        return Err(format!(
            "coordinator git {} failed in {}: {}",
            args.join(" "),
            cwd.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output.stdout)
}

fn exact_index_file(root: &Path) -> Result<Vec<u8>, String> {
    let index = coordinator_git_text(
        root,
        &["rev-parse", "--path-format=absolute", "--git-path", "index"],
    )?;
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&index)
        .map_err(|e| format!("securely open coordinator index {index}: {e}"))?;
    let metadata = file
        .metadata()
        .map_err(|e| format!("fstat coordinator index {index}: {e}"))?;
    if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } || metadata.nlink() != 1
    {
        return Err("coordinator index is not an EUID-owned single-link regular file".into());
    }
    let mut bytes = Vec::new();
    use std::io::Read;
    file.read_to_end(&mut bytes)
        .map_err(|e| format!("read coordinator index {index}: {e}"))?;
    Ok(bytes)
}

fn restore_exact_index_file(root: &Path, bytes: &[u8]) -> Result<(), String> {
    if exact_index_file(root)? == bytes {
        return Ok(());
    }
    let index = PathBuf::from(coordinator_git_text(
        root,
        &["rev-parse", "--path-format=absolute", "--git-path", "index"],
    )?);
    let current = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&index)
        .map_err(|e| format!("securely open current coordinator index: {e}"))?;
    let metadata = current
        .metadata()
        .map_err(|e| format!("fstat current coordinator index: {e}"))?;
    if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } || metadata.nlink() != 1
    {
        return Err(
            "current coordinator index is not an EUID-owned single-link regular file".into(),
        );
    }
    let parent = index
        .parent()
        .ok_or_else(|| "coordinator index has no parent".to_string())?;
    let staging = parent.join(format!(
        ".session-relay-index-restore-{}",
        crate::store::uuid_v4()
    ));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(metadata.mode() & 0o777)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&staging)
        .map_err(|e| format!("create exact index restore file: {e}"))?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|e| format!("persist exact index restore file: {e}"))?;
    fs::rename(&staging, &index)
        .map_err(|e| format!("atomically restore exact coordinator index: {e}"))?;
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(parent)
        .map_err(|e| format!("open coordinator index directory: {e}"))?;
    directory
        .sync_all()
        .map_err(|e| format!("fsync restored coordinator index: {e}"))?;
    if exact_index_file(root)? != bytes {
        return Err("restored coordinator index bytes differ from pre-integration bytes".into());
    }
    Ok(())
}

fn validate_coordinator_environment() -> Result<(), String> {
    for name in COORDINATOR_GIT_ENVIRONMENT {
        if std::env::var_os(name).is_some() {
            return Err(format!("coordinator integration refuses ambient {name}"));
        }
    }
    Ok(())
}

fn validate_branch_ref(value: &str, label: &str) -> Result<(), String> {
    if !value.starts_with("refs/heads/") {
        return Err(format!("{label} is not under refs/heads"));
    }
    let output = coordinator_git_output(Path::new("."), &["check-ref-format", value])?;
    if !output.status.success() {
        return Err(format!("{label} is not a valid full Git ref"));
    }
    Ok(())
}

fn open_exact_checkout(
    repository: &OpenedRepository,
    root: &Path,
    label: &str,
) -> Result<OpenedRepository, String> {
    if !root.is_absolute() {
        return Err(format!("{label} is not absolute"));
    }
    let canonical = fs::canonicalize(root)
        .map_err(|e| format!("canonicalize {label} {}: {e}", root.display()))?;
    if canonical != root {
        return Err(format!("{label} is not canonical or traverses a symlink"));
    }
    let opened = OpenedRepository::open(root)?;
    if opened.root != root {
        return Err(format!("{label} differs from its Git top-level"));
    }
    if opened.identity != repository.identity {
        return Err(format!(
            "{label} repository identity or object format differs"
        ));
    }
    Ok(opened)
}

fn exact_ref_head(root: &Path, branch_ref: &str) -> Result<String, String> {
    let symbolic = coordinator_git_text(root, &["symbolic-ref", "-q", "HEAD"])?;
    if symbolic != branch_ref {
        return Err(format!(
            "symbolic HEAD is {symbolic}, expected {branch_ref}"
        ));
    }
    let head = coordinator_git_text(root, &["rev-parse", "--verify", "HEAD"])?;
    let branch = coordinator_git_text(root, &["show-ref", "--verify", "--hash", branch_ref])?;
    if branch != head {
        return Err("symbolic branch ref and HEAD differ".into());
    }
    Ok(head)
}

fn exact_branch_ref_oid(
    repository: &OpenedRepository,
    root: &Path,
    branch_ref: &str,
) -> Result<String, String> {
    validate_branch_ref(branch_ref, "branch ref")?;
    let oid = coordinator_git_text(root, &["show-ref", "--verify", "--hash", branch_ref])?;
    repository.validate_oid(&oid)?;
    Ok(oid)
}

fn optional_branch_ref_oid(
    repository: &OpenedRepository,
    root: &Path,
    branch_ref: &str,
) -> Result<Option<String>, String> {
    validate_branch_ref(branch_ref, "branch ref")?;
    let probe = coordinator_git_output(root, &["show-ref", "--verify", "--quiet", branch_ref])?;
    if probe.status.success() {
        return exact_branch_ref_oid(repository, root, branch_ref).map(Some);
    }
    if probe.status.code() == Some(1) {
        return Ok(None);
    }
    Err(format!(
        "worker branch presence could not be proven: {}",
        String::from_utf8_lossy(&probe.stderr).trim()
    ))
}

fn command_is_quiet(root: &Path, args: &[&str], dirty_message: &str) -> Result<(), String> {
    let output = coordinator_git_output(root, args)?;
    if output.status.success() {
        return Ok(());
    }
    if output.status.code() == Some(1) {
        return Err(dirty_message.into());
    }
    Err(format!(
        "coordinator git {} could not prove cleanliness: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

pub fn pristine_git_state(
    repository: &OpenedRepository,
    root: &Path,
    branch_ref: &str,
    expected_head: &str,
) -> Result<PristineGitState, String> {
    repository.validate_unchanged()?;
    let head = exact_ref_head(root, branch_ref)?;
    let expected = repository.validate_oid(expected_head)?;
    if expected.as_str() != expected_head || head != expected_head {
        return Err("checkout HEAD differs from the exact expected OID".into());
    }
    let private = actual_private_git_dir(root)?;
    for marker in [
        "MERGE_HEAD",
        "REBASE_HEAD",
        "CHERRY_PICK_HEAD",
        "BISECT_START",
        "REVERT_HEAD",
    ] {
        if private.join(marker).exists() {
            return Err(format!(
                "checkout has forbidden Git operation marker {marker}"
            ));
        }
    }
    let status = coordinator_git_bytes(
        root,
        &["status", "--porcelain=v2", "-z", "--untracked-files=all"],
    )?;
    if !status.is_empty() {
        return Err("checkout status is not pristine".into());
    }
    command_is_quiet(
        root,
        &["diff", "--quiet", "--"],
        "checkout worktree differs from its index",
    )?;
    command_is_quiet(
        root,
        &["diff", "--cached", "--quiet", "HEAD", "--"],
        "checkout index differs from HEAD",
    )?;
    let head_tree = coordinator_git_text(
        root,
        &["rev-parse", "--verify", &format!("{head}^{{tree}}")],
    )?;
    let index_tree = head_tree.clone();
    repository.validate_oid(&head_tree)?;
    let index_entries = coordinator_git_bytes(root, &["ls-files", "--stage", "-z"])?;
    let index_file = exact_index_file(root)?;
    Ok(PristineGitState {
        head_oid: head,
        head_tree_oid: head_tree,
        index_tree_oid: index_tree,
        index_file_sha256: sha256::hex_digest(&index_file),
        index_entries_sha256: sha256::hex_digest(&index_entries),
        status_sha256: sha256::hex_digest(&status),
    })
}

fn validate_commit_object(
    repository: &OpenedRepository,
    root: &Path,
    oid: &str,
    label: &str,
) -> Result<(), String> {
    let parsed = repository.validate_oid(oid)?;
    if parsed.as_str() != oid {
        return Err(format!("{label} is not a canonical OID"));
    }
    let object_type = coordinator_git_text(root, &["cat-file", "-t", oid])?;
    if object_type != "commit" {
        return Err(format!("{label} is not a commit object"));
    }
    Ok(())
}

fn validate_ancestor(
    root: &Path,
    ancestor: &str,
    descendant: &str,
    label: &str,
) -> Result<(), String> {
    let output =
        coordinator_git_output(root, &["merge-base", "--is-ancestor", ancestor, descendant])?;
    if output.status.success() {
        return Ok(());
    }
    if output.status.code() == Some(1) {
        return Err(format!("{label} ancestry differs"));
    }
    Err(format!(
        "could not validate {label} ancestry: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

fn validate_produced_chain_at(
    repository: &OpenedRepository,
    request: &OrderedIntegrationRequest,
    git_root: &Path,
) -> Result<(), String> {
    if request.produced_commits.is_empty() {
        return Err("produced commit chain is empty; applied WIP must be index zero".into());
    }
    validate_commit_object(repository, git_root, &request.base_commit, "base commit")?;
    validate_commit_object(
        repository,
        git_root,
        &request.expected_worker_head,
        "expected worker HEAD",
    )?;
    let mut expected_parent = request.base_commit.as_str();
    for (index, commit) in request.produced_commits.iter().enumerate() {
        validate_commit_object(
            repository,
            git_root,
            &commit.oid,
            &format!("produced commit {index}"),
        )?;
        let parent = repository.validate_oid(&commit.parent_oid)?;
        if parent.as_str() != commit.parent_oid {
            return Err(format!("produced commit {index} parent is not canonical"));
        }
        let expected_source = if index == 0 { "applied_wip" } else { "worker" };
        if commit.source != expected_source {
            return Err(format!(
                "produced commit {index} source must be {expected_source}"
            ));
        }
        if commit.parent_oid != expected_parent {
            return Err(format!(
                "produced commit {index} is out of strict linear order"
            ));
        }
        let row =
            coordinator_git_text(git_root, &["rev-list", "--parents", "-n", "1", &commit.oid])?;
        let fields = row.split_whitespace().collect::<Vec<_>>();
        if fields.len() != 2 || fields[0] != commit.oid || fields[1] != commit.parent_oid {
            return Err(format!(
                "produced commit {index} is not a single-parent commit with its declared parent"
            ));
        }
        expected_parent = &commit.oid;
    }
    if expected_parent != request.expected_worker_head {
        return Err("last produced commit differs from expected worker HEAD".into());
    }
    validate_ancestor(
        git_root,
        &request.base_commit,
        &request.expected_worker_head,
        "worker base",
    )?;
    let range = format!("{}..{}", request.base_commit, request.expected_worker_head);
    let actual =
        coordinator_git_text(git_root, &["rev-list", "--reverse", "--topo-order", &range])?
            .lines()
            .map(str::to_string)
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>();
    let declared = request
        .produced_commits
        .iter()
        .map(|commit| commit.oid.clone())
        .collect::<Vec<_>>();
    if actual != declared {
        return Err("produced commit list is not the complete oldest-first worker history".into());
    }
    Ok(())
}

fn validate_produced_chain(
    repository: &OpenedRepository,
    request: &OrderedIntegrationRequest,
) -> Result<(), String> {
    validate_produced_chain_at(repository, request, &request.worker_root)
}

fn prepare_ordered_integration(
    repository: &OpenedRepository,
    request: &OrderedIntegrationRequest,
) -> Result<PreparedIntegration, String> {
    validate_coordinator_environment()?;
    repository.validate_unchanged()?;
    validate_branch_ref(&request.integration_branch_ref, "integration branch ref")?;
    validate_branch_ref(&request.worker_branch_ref, "worker branch ref")?;
    if request.integration_root == request.worker_root {
        return Err("integration and worker roots must be distinct".into());
    }
    if request.integration_branch_ref == request.worker_branch_ref {
        return Err("integration and worker branch refs must be distinct".into());
    }
    let integration =
        open_exact_checkout(repository, &request.integration_root, "integration root")?;
    let worker = open_exact_checkout(repository, &request.worker_root, "worker root")?;
    validate_commit_object(
        repository,
        &integration.root,
        &request.expected_integration_head,
        "expected integration HEAD",
    )?;
    let integration_index_bytes = exact_index_file(&integration.root)?;
    let worker_index_bytes = exact_index_file(&worker.root)?;
    let mut integration_state = pristine_git_state(
        &integration,
        &integration.root,
        &request.integration_branch_ref,
        &request.expected_integration_head,
    )?;
    integration_state.index_file_sha256 = sha256::hex_digest(&integration_index_bytes);
    let mut worker_state = pristine_git_state(
        &worker,
        &worker.root,
        &request.worker_branch_ref,
        &request.expected_worker_head,
    )?;
    worker_state.index_file_sha256 = sha256::hex_digest(&worker_index_bytes);
    validate_produced_chain(repository, request)?;
    validate_ancestor(
        &integration.root,
        &request.base_commit,
        &request.expected_integration_head,
        "integration base",
    )?;
    Ok(PreparedIntegration {
        integration,
        worker,
        integration_state,
        worker_state,
        integration_index_bytes,
        worker_index_bytes,
    })
}

pub fn capture_ordered_integration_prestate(
    repository: &OpenedRepository,
    request: &OrderedIntegrationRequest,
) -> Result<OrderedIntegrationPrestate, String> {
    let prepared = prepare_ordered_integration(repository, request)?;
    let snapshot = OrderedIntegrationPrestate {
        integration_state: prepared.integration_state.clone(),
        integration_index_bytes: prepared.integration_index_bytes.clone(),
    };
    restore_exact_index_file(
        &prepared.integration.root,
        &prepared.integration_index_bytes,
    )?;
    restore_exact_index_file(&prepared.worker.root, &prepared.worker_index_bytes)?;
    Ok(snapshot)
}

pub fn validate_ordered_integration(
    repository: &OpenedRepository,
    request: &OrderedIntegrationRequest,
) -> Result<(PristineGitState, PristineGitState), String> {
    let prepared = prepare_ordered_integration(repository, request)?;
    restore_exact_index_file(
        &prepared.integration.root,
        &prepared.integration_index_bytes,
    )?;
    restore_exact_index_file(&prepared.worker.root, &prepared.worker_index_bytes)?;
    Ok((prepared.integration_state, prepared.worker_state))
}

fn parse_unmerged_paths(bytes: &[u8]) -> Result<Vec<String>, String> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    if !bytes.ends_with(&[0]) {
        return Err("unmerged path output is truncated".into());
    }
    let mut paths = Vec::new();
    for field in bytes[..bytes.len() - 1].split(|byte| *byte == 0) {
        if field.is_empty() {
            return Err("unmerged path output contains an empty path".into());
        }
        let path = std::str::from_utf8(field)
            .map_err(|_| "unmerged path is not UTF-8".to_string())?
            .to_string();
        schema::RelPath::parse(&path)?;
        paths.push(path);
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn restore_preintegration_state(
    prepared: &PreparedIntegration,
    request: &OrderedIntegrationRequest,
    current_head: &str,
) -> Result<(), String> {
    let root = &prepared.integration.root;
    let current = pristine_git_state(
        &prepared.integration,
        root,
        &request.integration_branch_ref,
        current_head,
    )?;
    if current.head_oid != prepared.integration_state.head_oid {
        coordinator_git_text(
            root,
            &[
                "update-ref",
                "--no-deref",
                &request.integration_branch_ref,
                &prepared.integration_state.head_oid,
                current_head,
            ],
        )?;
        coordinator_git_text(
            root,
            &[
                "read-tree",
                "--reset",
                "-u",
                &prepared.integration_state.head_oid,
            ],
        )?;
    }
    restore_exact_index_file(root, &prepared.integration_index_bytes)?;
    let restored = pristine_git_state(
        &prepared.integration,
        root,
        &request.integration_branch_ref,
        &prepared.integration_state.head_oid,
    )?;
    if !same_pristine_semantics(&restored, &prepared.integration_state) {
        return Err(format!(
            "integration rollback did not restore the exact clean pre-head/index/tree/status: pre={:?}, restored={restored:?}",
            prepared.integration_state
        ));
    }
    restore_exact_index_file(root, &prepared.integration_index_bytes)?;
    Ok(())
}

fn settle_failed_cherry_pick(
    prepared: &PreparedIntegration,
    request: &OrderedIntegrationRequest,
    current_head: &str,
    command_error: &str,
) -> Result<CoordinatorIntegrationResult, String> {
    let root = &prepared.integration.root;
    let paths_result = coordinator_git_bytes(
        root,
        &["diff", "--name-only", "--diff-filter=U", "-z", "--"],
    )
    .and_then(|bytes| parse_unmerged_paths(&bytes));
    let marker = actual_private_git_dir(root)?.join("CHERRY_PICK_HEAD");
    if marker.exists() {
        if exact_ref_head(root, &request.integration_branch_ref)? != current_head {
            return Err(format!(
                "cherry-pick failed and integration ref drifted before abort: {command_error}"
            ));
        }
        let abort = coordinator_git_output(
            root,
            &["-c", "core.hooksPath=/dev/null", "cherry-pick", "--abort"],
        )?;
        if !abort.status.success() {
            return Err(format!(
                "cherry-pick abort was ambiguous after {command_error}: {}",
                String::from_utf8_lossy(&abort.stderr).trim()
            ));
        }
    } else {
        let unchanged = pristine_git_state(
            &prepared.integration,
            root,
            &request.integration_branch_ref,
            current_head,
        );
        if unchanged.is_err() {
            return Err(format!(
                "cherry-pick failed without an abortable operation and state is ambiguous: {command_error}"
            ));
        }
    }
    restore_preintegration_state(prepared, request, current_head)?;
    let worker_after = pristine_git_state(
        &prepared.worker,
        &prepared.worker.root,
        &request.worker_branch_ref,
        &request.expected_worker_head,
    )?;
    if !same_pristine_semantics(&worker_after, &prepared.worker_state) {
        return Err("worker branch/work changed while restoring integration conflict".into());
    }
    restore_exact_index_file(&prepared.worker.root, &prepared.worker_index_bytes)?;
    let paths=paths_result.map_err(|error|format!("integration was restored but conflict paths were ambiguous after {command_error}: {error}"))?;
    if paths.is_empty() {
        return Err(format!(
            "integration was restored after a non-conflict cherry-pick failure: {command_error}"
        ));
    }
    Ok(CoordinatorIntegrationResult {
        outcome: CoordinatorIntegrationOutcome::NeedsUserAction,
        pre_head: prepared.integration_state.head_oid.clone(),
        post_head: prepared.integration_state.head_oid.clone(),
        output_oids: Vec::new(),
        conflict_paths: paths,
    })
}

fn commit_patch_id(root: &Path, parent: &str, commit: &str) -> Result<String, String> {
    let patch = coordinator_git_bytes(root, &["diff", "--binary", parent, commit, "--"])?;
    let mut command = Command::new("git");
    command.args(["patch-id", "--stable"]).current_dir(root);
    for name in COORDINATOR_GIT_ENVIRONMENT {
        command.env_remove(name);
    }
    let output = command_with_input(command, &patch, "coordinator patch-id")?;
    if !output.status.success() {
        return Err(format!(
            "coordinator patch-id failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let text = String::from_utf8(output.stdout)
        .map_err(|_| "coordinator patch-id output was not UTF-8".to_string())?;
    text.split_whitespace()
        .next()
        .map(str::to_string)
        .ok_or_else(|| "coordinator patch-id output was empty".to_string())
}

fn prove_cherry_pick_output(
    repository: &OpenedRepository,
    root: &Path,
    source: &str,
    parent: &str,
    output: &str,
) -> Result<(), String> {
    validate_commit_object(repository, root, output, "recovered integration output")?;
    let row = coordinator_git_text(root, &["rev-list", "--parents", "-n", "1", output])?;
    let fields = row.split_whitespace().collect::<Vec<_>>();
    if fields.len() != 2 || fields[0] != output || fields[1] != parent {
        return Err(
            "recovered integration output is not the exact next single-parent commit".into(),
        );
    }
    let source_parent =
        coordinator_git_text(root, &["rev-parse", "--verify", &format!("{source}^")])?;
    let source_tree = coordinator_git_text(
        root,
        &["rev-parse", "--verify", &format!("{source}^{{tree}}")],
    )?;
    let source_parent_tree = coordinator_git_text(
        root,
        &[
            "rev-parse",
            "--verify",
            &format!("{source_parent}^{{tree}}"),
        ],
    )?;
    if source_tree == source_parent_tree {
        let output_tree = coordinator_git_text(
            root,
            &["rev-parse", "--verify", &format!("{output}^{{tree}}")],
        )?;
        let parent_tree = coordinator_git_text(
            root,
            &["rev-parse", "--verify", &format!("{parent}^{{tree}}")],
        )?;
        if output_tree != parent_tree {
            return Err("recovered empty integration output changed the parent tree".into());
        }
    } else if commit_patch_id(root, &source_parent, source)?
        != commit_patch_id(root, parent, output)?
    {
        return Err(
            "recovered integration output patch differs from the requested cherry-pick".into(),
        );
    }
    let format = "--format=%an%x00%ae%x00%aI%x00%B";
    if coordinator_git_bytes(root, &["show", "-s", format, source])?
        != coordinator_git_bytes(root, &["show", "-s", format, output])?
    {
        return Err(
            "recovered integration output author or message differs from the requested cherry-pick"
                .into(),
        );
    }
    Ok(())
}

fn observed_integration_outputs(
    repository: &OpenedRepository,
    request: &OrderedIntegrationRequest,
    current: &str,
) -> Result<Vec<String>, String> {
    if current == request.expected_integration_head {
        return Ok(Vec::new());
    }
    validate_ancestor(
        &request.integration_root,
        &request.expected_integration_head,
        current,
        "queued integration output",
    )?;
    let range = format!("{}..{current}", request.expected_integration_head);
    let outputs = coordinator_git_text(
        &request.integration_root,
        &["rev-list", "--first-parent", "--reverse", &range],
    )?
    .lines()
    .map(str::to_string)
    .filter(|line| !line.is_empty())
    .collect::<Vec<_>>();
    if outputs.len() > request.produced_commits.len() {
        return Err("queued integration advanced beyond the requested commit chain".into());
    }
    let mut parent = request.expected_integration_head.as_str();
    for (index, output) in outputs.iter().enumerate() {
        prove_cherry_pick_output(
            repository,
            &request.integration_root,
            &request.produced_commits[index].oid,
            parent,
            output,
        )?;
        parent = output;
    }
    Ok(outputs)
}

pub fn rollback_queued_integration<F>(
    repository: &OpenedRepository,
    request: &OrderedIntegrationRequest,
    prestate: &OrderedIntegrationPrestate,
    durable_outputs: &[String],
    mut persist_recovered: F,
) -> Result<(), String>
where
    F: FnMut(&[String]) -> Result<(), String>,
{
    validate_coordinator_environment()?;
    repository.validate_unchanged()?;
    if prestate.integration_state.head_oid != request.expected_integration_head
        || sha256::hex_digest(&prestate.integration_index_bytes)
            != prestate.integration_state.index_file_sha256
    {
        return Err(
            "durable integration prestate differs from the queued request or index evidence".into(),
        );
    }
    validate_branch_ref(&request.integration_branch_ref, "integration branch ref")?;
    validate_branch_ref(&request.worker_branch_ref, "worker branch ref")?;
    let integration =
        open_exact_checkout(repository, &request.integration_root, "integration root")?;
    let worker = open_exact_checkout(repository, &request.worker_root, "worker root")?;
    validate_produced_chain(repository, request)?;
    let worker_index = exact_index_file(&worker.root)?;
    let worker_state = pristine_git_state(
        &worker,
        &worker.root,
        &request.worker_branch_ref,
        &request.expected_worker_head,
    )?;
    let current = exact_ref_head(&integration.root, &request.integration_branch_ref)?;
    let outputs = observed_integration_outputs(repository, request, &current)?;
    if outputs.is_empty() && !durable_outputs.is_empty() {
        if durable_outputs.len() > request.produced_commits.len() {
            return Err("durable per-step integration evidence exceeds the requested chain".into());
        }
        let mut parent = request.expected_integration_head.as_str();
        for (index, output) in durable_outputs.iter().enumerate() {
            prove_cherry_pick_output(
                repository,
                &integration.root,
                &request.produced_commits[index].oid,
                parent,
                output,
            )?;
            parent = output;
        }
    } else {
        if durable_outputs.len() > outputs.len()
            || durable_outputs != &outputs[..durable_outputs.len()]
        {
            return Err(
                "durable per-step integration evidence differs from the queued Git history".into(),
            );
        }
        if outputs.len() > durable_outputs.len() {
            persist_recovered(&outputs)?
        }
    }
    let marker = actual_private_git_dir(&integration.root)?.join("CHERRY_PICK_HEAD");
    let mut repair_prehead = false;
    if marker.exists() {
        if outputs.len() >= request.produced_commits.len() {
            return Err(
                "queued integration has a cherry-pick marker after every requested output".into(),
            );
        }
        let marked = fs::read_to_string(&marker)
            .map_err(|e| format!("read queued cherry-pick marker: {e}"))?;
        if marked.trim() != request.produced_commits[outputs.len()].oid {
            return Err("queued cherry-pick marker differs from the next requested commit".into());
        }
        let abort = coordinator_git_output(
            &integration.root,
            &["-c", "core.hooksPath=/dev/null", "cherry-pick", "--abort"],
        )?;
        if !abort.status.success() {
            return Err(format!(
                "abort queued cherry-pick before exact rollback: {}",
                String::from_utf8_lossy(&abort.stderr).trim()
            ));
        }
    } else if pristine_git_state(
        &integration,
        &integration.root,
        &request.integration_branch_ref,
        &current,
    )
    .is_err()
    {
        if current != request.expected_integration_head || durable_outputs.is_empty() {
            return Err("queued integration checkout is dirty without durable proof of an interrupted exact rollback".into());
        }
        repair_prehead = true;
    }
    let after_abort = exact_ref_head(&integration.root, &request.integration_branch_ref)?;
    if after_abort != current {
        return Err("queued cherry-pick abort changed the proven partial integration HEAD".into());
    }
    if current != request.expected_integration_head {
        coordinator_git_text(
            &integration.root,
            &[
                "update-ref",
                "--no-deref",
                &request.integration_branch_ref,
                &request.expected_integration_head,
                &current,
            ],
        )?;
        coordinator_git_text(
            &integration.root,
            &[
                "read-tree",
                "--reset",
                "-u",
                &request.expected_integration_head,
            ],
        )?;
    } else if repair_prehead {
        coordinator_git_text(
            &integration.root,
            &[
                "read-tree",
                "--reset",
                "-u",
                &request.expected_integration_head,
            ],
        )?;
    }
    restore_exact_index_file(&integration.root, &prestate.integration_index_bytes)?;
    let mut restored = pristine_git_state(
        &integration,
        &integration.root,
        &request.integration_branch_ref,
        &request.expected_integration_head,
    )?;
    restored.index_file_sha256 = prestate.integration_state.index_file_sha256.clone();
    if !same_pristine_semantics(&restored, &prestate.integration_state) {
        return Err(
            "queued integration rollback did not restore the durable clean prestate semantics"
                .into(),
        );
    }
    restore_exact_index_file(&integration.root, &prestate.integration_index_bytes)?;
    if exact_index_file(&integration.root)? != prestate.integration_index_bytes {
        return Err(
            "queued integration rollback did not restore the exact durable index bytes".into(),
        );
    }
    let worker_after = pristine_git_state(
        &worker,
        &worker.root,
        &request.worker_branch_ref,
        &request.expected_worker_head,
    )?;
    if !same_pristine_semantics(&worker_after, &worker_state) {
        return Err("worker checkout changed during queued integration rollback".into());
    }
    restore_exact_index_file(&worker.root, &worker_index)?;
    Ok(())
}

pub fn verify_queued_integration_outputs(
    repository: &OpenedRepository,
    request: &OrderedIntegrationRequest,
    prestate: &OrderedIntegrationPrestate,
    outputs: &[String],
) -> Result<(), String> {
    validate_coordinator_environment()?;
    repository.validate_unchanged()?;
    if prestate.integration_state.head_oid != request.expected_integration_head
        || sha256::hex_digest(&prestate.integration_index_bytes)
            != prestate.integration_state.index_file_sha256
    {
        return Err("durable integration pre-head or index differs from the queued request".into());
    }
    validate_produced_chain(repository, request)?;
    let integration =
        open_exact_checkout(repository, &request.integration_root, "integration root")?;
    let worker = open_exact_checkout(repository, &request.worker_root, "worker root")?;
    let current = exact_ref_head(&integration.root, &request.integration_branch_ref)?;
    let observed = observed_integration_outputs(repository, request, &current)?;
    if observed != outputs {
        return Err(
            "durable integration receipt outputs differ from the proven Git history".into(),
        );
    }
    pristine_git_state(
        &integration,
        &integration.root,
        &request.integration_branch_ref,
        &current,
    )?;
    pristine_git_state(
        &worker,
        &worker.root,
        &request.worker_branch_ref,
        &request.expected_worker_head,
    )?;
    Ok(())
}

pub fn integrate_ordered_with_step<F>(
    repository: &OpenedRepository,
    request: &OrderedIntegrationRequest,
    mut persist_step: F,
) -> Result<CoordinatorIntegrationResult, String>
where
    F: FnMut(&[String]) -> Result<(), String>,
{
    let prepared = prepare_ordered_integration(repository, request)?;
    let mut current = prepared.integration_state.head_oid.clone();
    let mut outputs = Vec::with_capacity(request.produced_commits.len());
    for commit in &request.produced_commits {
        let before = pristine_git_state(
            &prepared.integration,
            &prepared.integration.root,
            &request.integration_branch_ref,
            &current,
        )?;
        let output = coordinator_git_output(
            &prepared.integration.root,
            &[
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "commit.gpgSign=false",
                "cherry-pick",
                "--no-gpg-sign",
                "--allow-empty",
                "--keep-redundant-commits",
                &commit.oid,
            ],
        )?;
        if !output.status.success() {
            let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let result = settle_failed_cherry_pick(&prepared, request, &current, &message)?;
            persist_step(&[])?;
            return Ok(result);
        }
        let next = exact_ref_head(&prepared.integration.root, &request.integration_branch_ref)?;
        validate_commit_object(
            &prepared.integration,
            &prepared.integration.root,
            &next,
            "integration output",
        )?;
        let row = coordinator_git_text(
            &prepared.integration.root,
            &["rev-list", "--parents", "-n", "1", &next],
        )?;
        let fields = row.split_whitespace().collect::<Vec<_>>();
        if fields.len() != 2 || fields[0] != next || fields[1] != current {
            return Err("integration output is not the exact next single-parent commit".into());
        }
        let after = pristine_git_state(
            &prepared.integration,
            &prepared.integration.root,
            &request.integration_branch_ref,
            &next,
        )?;
        if before.head_oid != current || after.head_oid == current {
            return Err("integration cherry-pick did not advance exactly once".into());
        }
        outputs.push(next.clone());
        persist_step(&outputs)?;
        current = next;
    }
    if outputs.len() != request.produced_commits.len() {
        return Err("integration output cardinality differs from produced chain".into());
    }
    let worker_after = pristine_git_state(
        &prepared.worker,
        &prepared.worker.root,
        &request.worker_branch_ref,
        &request.expected_worker_head,
    )?;
    if !same_pristine_semantics(&worker_after, &prepared.worker_state) {
        return Err("worker branch/work changed during coordinator integration".into());
    }
    restore_exact_index_file(&prepared.worker.root, &prepared.worker_index_bytes)?;
    Ok(CoordinatorIntegrationResult {
        outcome: CoordinatorIntegrationOutcome::Integrated,
        pre_head: prepared.integration_state.head_oid,
        post_head: current,
        output_oids: outputs,
        conflict_paths: Vec::new(),
    })
}

pub fn integrate_ordered(
    repository: &OpenedRepository,
    request: &OrderedIntegrationRequest,
) -> Result<CoordinatorIntegrationResult, String> {
    integrate_ordered_with_step(repository, request, |_| Ok(()))
}

pub fn reject_ordered(
    repository: &OpenedRepository,
    request: &OrderedIntegrationRequest,
) -> Result<CoordinatorIntegrationResult, String> {
    let prepared = prepare_ordered_integration(repository, request)?;
    let integration_after = pristine_git_state(
        &prepared.integration,
        &prepared.integration.root,
        &request.integration_branch_ref,
        &request.expected_integration_head,
    )?;
    let worker_after = pristine_git_state(
        &prepared.worker,
        &prepared.worker.root,
        &request.worker_branch_ref,
        &request.expected_worker_head,
    )?;
    if !same_pristine_semantics(&integration_after, &prepared.integration_state)
        || !same_pristine_semantics(&worker_after, &prepared.worker_state)
    {
        return Err("rejection validation observed checkout drift".into());
    }
    restore_exact_index_file(
        &prepared.integration.root,
        &prepared.integration_index_bytes,
    )?;
    restore_exact_index_file(&prepared.worker.root, &prepared.worker_index_bytes)?;
    Ok(CoordinatorIntegrationResult {
        outcome: CoordinatorIntegrationOutcome::Rejected,
        pre_head: prepared.integration_state.head_oid.clone(),
        post_head: prepared.integration_state.head_oid,
        output_oids: Vec::new(),
        conflict_paths: Vec::new(),
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GitCleanupProof {
    Integrated(CoordinatorIntegrationResult),
    Rejected(CoordinatorIntegrationResult),
    Retained { retention_proof_sha256: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitCleanupResult {
    pub removed_worktree: PathBuf,
    pub removed_branch_ref: String,
    pub retained_head_oid: String,
}

fn validate_output_chain(
    repository: &OpenedRepository,
    root: &Path,
    pre_head: &str,
    post_head: &str,
    outputs: &[String],
    expected_len: usize,
) -> Result<(), String> {
    validate_commit_object(repository, root, pre_head, "integration pre-head")?;
    validate_commit_object(repository, root, post_head, "integration post-head")?;
    if outputs.len() != expected_len {
        return Err(
            "integrated cleanup proof output cardinality differs from produced chain".into(),
        );
    }
    let mut parent = pre_head;
    for (index, oid) in outputs.iter().enumerate() {
        validate_commit_object(
            repository,
            root,
            oid,
            &format!("integration output {index}"),
        )?;
        let row = coordinator_git_text(root, &["rev-list", "--parents", "-n", "1", oid])?;
        let fields = row.split_whitespace().collect::<Vec<_>>();
        if fields.len() != 2 || fields[0] != oid || fields[1] != parent {
            return Err(format!(
                "integration output {index} is not in exact receipt order"
            ));
        }
        parent = oid;
    }
    if parent != post_head {
        return Err("integrated cleanup proof last output differs from post-head".into());
    }
    Ok(())
}

fn verify_ordered_cleanup_at(
    repository: &OpenedRepository,
    request: &OrderedIntegrationRequest,
    proof: &GitCleanupProof,
    expected_integration_checkout_head: &str,
) -> Result<(), String> {
    validate_coordinator_environment()?;
    repository.validate_unchanged()?;
    validate_branch_ref(&request.integration_branch_ref, "integration branch ref")?;
    validate_branch_ref(&request.worker_branch_ref, "worker branch ref")?;
    let integration =
        open_exact_checkout(repository, &request.integration_root, "integration root")?;
    let worker = open_exact_checkout(repository, &request.worker_root, "worker root")?;
    let integration_index_bytes = exact_index_file(&integration.root)?;
    let worker_index_bytes = exact_index_file(&worker.root)?;
    let worker_state = pristine_git_state(
        &worker,
        &worker.root,
        &request.worker_branch_ref,
        &request.expected_worker_head,
    )?;
    if worker_state.head_oid != request.expected_worker_head {
        return Err("cleanup worker HEAD differs".into());
    }
    validate_produced_chain(repository, request)?;
    pristine_git_state(
        &integration,
        &integration.root,
        &request.integration_branch_ref,
        expected_integration_checkout_head,
    )?;
    match proof {
        GitCleanupProof::Integrated(result) => {
            if result.outcome != CoordinatorIntegrationOutcome::Integrated
                || result.pre_head != request.expected_integration_head
                || !result.conflict_paths.is_empty()
            {
                return Err("integrated cleanup proof has an invalid outcome or pre-head".into());
            }
            validate_output_chain(
                &integration,
                &integration.root,
                &result.pre_head,
                &result.post_head,
                &result.output_oids,
                request.produced_commits.len(),
            )?;
            validate_ancestor(
                &integration.root,
                &result.post_head,
                expected_integration_checkout_head,
                "integrated receipt post-head",
            )?;
        }
        GitCleanupProof::Rejected(result) => {
            if result.outcome != CoordinatorIntegrationOutcome::Rejected
                || result.pre_head != request.expected_integration_head
                || result.post_head != result.pre_head
                || result.post_head != expected_integration_checkout_head
                || !result.output_oids.is_empty()
                || !result.conflict_paths.is_empty()
            {
                return Err("rejected cleanup proof is not exact and mutation-free".into());
            }
        }
        GitCleanupProof::Retained {
            retention_proof_sha256,
        } => {
            schema::Sha256Digest::parse(retention_proof_sha256)?;
            if request.expected_integration_head != expected_integration_checkout_head {
                return Err(
                    "retained cleanup integration HEAD differs from its exact request".into(),
                );
            }
        }
    }
    restore_exact_index_file(&integration.root, &integration_index_bytes)?;
    restore_exact_index_file(&worker.root, &worker_index_bytes)?;
    Ok(())
}

fn verify_ordered_cleanup_after_worktree_removal(
    repository: &OpenedRepository,
    request: &OrderedIntegrationRequest,
    proof: &GitCleanupProof,
    expected_integration_checkout_head: &str,
) -> Result<(), String> {
    validate_coordinator_environment()?;
    repository.validate_unchanged()?;
    validate_branch_ref(&request.integration_branch_ref, "integration branch ref")?;
    validate_branch_ref(&request.worker_branch_ref, "worker branch ref")?;
    let integration =
        open_exact_checkout(repository, &request.integration_root, "integration root")?;
    let integration_index_bytes = exact_index_file(&integration.root)?;
    validate_produced_chain_at(repository, request, &integration.root)?;
    pristine_git_state(
        &integration,
        &integration.root,
        &request.integration_branch_ref,
        expected_integration_checkout_head,
    )?;
    match proof {
        GitCleanupProof::Integrated(result) => {
            if result.outcome != CoordinatorIntegrationOutcome::Integrated
                || result.pre_head != request.expected_integration_head
                || !result.conflict_paths.is_empty()
            {
                return Err("integrated cleanup proof has an invalid outcome or pre-head".into());
            }
            validate_output_chain(
                &integration,
                &integration.root,
                &result.pre_head,
                &result.post_head,
                &result.output_oids,
                request.produced_commits.len(),
            )?;
            validate_ancestor(
                &integration.root,
                &result.post_head,
                expected_integration_checkout_head,
                "integrated receipt post-head",
            )?;
        }
        GitCleanupProof::Rejected(result) => {
            if result.outcome != CoordinatorIntegrationOutcome::Rejected
                || result.pre_head != request.expected_integration_head
                || result.post_head != result.pre_head
                || result.post_head != expected_integration_checkout_head
                || !result.output_oids.is_empty()
                || !result.conflict_paths.is_empty()
            {
                return Err("rejected cleanup proof is not exact and mutation-free".into());
            }
        }
        GitCleanupProof::Retained {
            retention_proof_sha256,
        } => {
            schema::Sha256Digest::parse(retention_proof_sha256)?;
            if request.expected_integration_head != expected_integration_checkout_head {
                return Err(
                    "retained cleanup integration HEAD differs from its exact request".into(),
                );
            }
        }
    }
    restore_exact_index_file(&integration.root, &integration_index_bytes)
}

pub fn verify_integrated_cleanup_at(
    repository: &OpenedRepository,
    request: &OrderedIntegrationRequest,
    proof: &GitCleanupProof,
    expected_integration_checkout_head: &str,
) -> Result<(), String> {
    if !matches!(proof, GitCleanupProof::Integrated(_)) {
        return Err("current-head cleanup is only valid for integrated proof".into());
    }
    verify_ordered_cleanup_at(
        repository,
        request,
        proof,
        expected_integration_checkout_head,
    )
}

pub fn verify_ordered_cleanup(
    repository: &OpenedRepository,
    request: &OrderedIntegrationRequest,
    proof: &GitCleanupProof,
) -> Result<(), String> {
    let expected_integration_checkout_head = match proof {
        GitCleanupProof::Integrated(result) | GitCleanupProof::Rejected(result) => {
            result.post_head.as_str()
        }
        GitCleanupProof::Retained { .. } => request.expected_integration_head.as_str(),
    };
    verify_ordered_cleanup_at(
        repository,
        request,
        proof,
        expected_integration_checkout_head,
    )
}

fn cleanup_ordered_worktree_at(
    repository: &OpenedRepository,
    request: &OrderedIntegrationRequest,
    proof: &GitCleanupProof,
    expected_integration_checkout_head: &str,
) -> Result<GitCleanupResult, String> {
    if request.worker_root.exists() {
        verify_ordered_cleanup_at(
            repository,
            request,
            proof,
            expected_integration_checkout_head,
        )?;
        let root_text = request
            .worker_root
            .to_str()
            .ok_or_else(|| "worker root is not UTF-8".to_string())?;
        coordinator_git_text(&repository.root, &["worktree", "unlock", root_text])?;
        if let Err(error) = verify_ordered_cleanup_at(
            repository,
            request,
            proof,
            expected_integration_checkout_head,
        ) {
            return Err(format!(
                "cleanup proof drifted after unlock; worktree retained: {error}"
            ));
        }
        coordinator_git_text(&repository.root, &["worktree", "remove", root_text])?;
        if request.worker_root.exists() {
            return Err("Git reported worktree removal but the exact worker root remains".into());
        }
    } else {
        verify_ordered_cleanup_after_worktree_removal(
            repository,
            request,
            proof,
            expected_integration_checkout_head,
        )?;
    }
    match optional_branch_ref_oid(repository, &repository.root, &request.worker_branch_ref)? {
        Some(branch) if branch == request.expected_worker_head => {
            coordinator_git_text(
                &repository.root,
                &[
                    "update-ref",
                    "--no-deref",
                    "-d",
                    &request.worker_branch_ref,
                    &request.expected_worker_head,
                ],
            )?;
        }
        Some(_) => return Err("worker ref drifted after worktree removal; ref retained".into()),
        None => {}
    }
    if optional_branch_ref_oid(repository, &repository.root, &request.worker_branch_ref)?.is_some()
    {
        return Err("worker branch ref remains after cleanup".into());
    }
    Ok(GitCleanupResult {
        removed_worktree: request.worker_root.clone(),
        removed_branch_ref: request.worker_branch_ref.clone(),
        retained_head_oid: request.expected_worker_head.clone(),
    })
}

pub fn cleanup_ordered_worktree(
    repository: &OpenedRepository,
    request: &OrderedIntegrationRequest,
    proof: &GitCleanupProof,
) -> Result<GitCleanupResult, String> {
    let expected_integration_checkout_head = match proof {
        GitCleanupProof::Integrated(result) | GitCleanupProof::Rejected(result) => {
            result.post_head.as_str()
        }
        GitCleanupProof::Retained { .. } => request.expected_integration_head.as_str(),
    };
    cleanup_ordered_worktree_at(
        repository,
        request,
        proof,
        expected_integration_checkout_head,
    )
}

pub fn cleanup_integrated_worktree_at(
    repository: &OpenedRepository,
    request: &OrderedIntegrationRequest,
    proof: &GitCleanupProof,
    expected_integration_checkout_head: &str,
) -> Result<GitCleanupResult, String> {
    if !matches!(proof, GitCleanupProof::Integrated(_)) {
        return Err("current-head cleanup is only valid for integrated proof".into());
    }
    cleanup_ordered_worktree_at(
        repository,
        request,
        proof,
        expected_integration_checkout_head,
    )
}

pub fn cleanup_prelaunch_worktree(
    repository: &OpenedRepository,
    worker_root: &Path,
    worker_branch_ref: &str,
    expected_worker_head: &str,
) -> Result<GitCleanupResult, String> {
    validate_coordinator_environment()?;
    repository.validate_unchanged()?;
    validate_branch_ref(worker_branch_ref, "prelaunch worker branch ref")?;
    let worker = open_exact_checkout(repository, worker_root, "prelaunch worker root")?;
    let index = exact_index_file(worker_root)?;
    let state = pristine_git_state(
        &worker,
        worker_root,
        worker_branch_ref,
        expected_worker_head,
    )?;
    restore_exact_index_file(worker_root, &index)?;
    let root_text = worker_root
        .to_str()
        .ok_or_else(|| "prelaunch worker root is not UTF-8".to_string())?;
    coordinator_git_text(&repository.root, &["worktree", "unlock", root_text])?;
    let after = pristine_git_state(
        &worker,
        worker_root,
        worker_branch_ref,
        expected_worker_head,
    )?;
    if !same_pristine_semantics(&state, &after) {
        return Err("prelaunch checkout drifted after unlock; worktree retained".into());
    }
    restore_exact_index_file(worker_root, &index)?;
    coordinator_git_text(&repository.root, &["worktree", "remove", root_text])?;
    if worker_root.exists() {
        return Err("prelaunch worktree remains after exact cleanup".into());
    }
    if exact_branch_ref_oid(repository, &repository.root, worker_branch_ref)?
        != expected_worker_head
    {
        return Err("prelaunch worker ref drifted after worktree removal; ref retained".into());
    }
    coordinator_git_text(
        &repository.root,
        &[
            "update-ref",
            "--no-deref",
            "-d",
            worker_branch_ref,
            expected_worker_head,
        ],
    )?;
    let probe = coordinator_git_output(
        &repository.root,
        &["show-ref", "--verify", "--quiet", worker_branch_ref],
    )?;
    if probe.status.success() || probe.status.code() != Some(1) {
        return Err("prelaunch worker branch absence could not be proven".into());
    }
    Ok(GitCleanupResult {
        removed_worktree: worker_root.to_path_buf(),
        removed_branch_ref: worker_branch_ref.into(),
        retained_head_oid: expected_worker_head.into(),
    })
}

pub fn verify_retained_worktree(
    repository: &OpenedRepository,
    worker_root: &Path,
    worker_branch_ref: &str,
    expected_worker_head: &str,
    retention_proof_sha256: &str,
) -> Result<WorktreeIdentityV1, String> {
    validate_coordinator_environment()?;
    repository.validate_unchanged()?;
    schema::Sha256Digest::parse(retention_proof_sha256)?;
    validate_branch_ref(worker_branch_ref, "retained worker branch ref")?;
    let worker = open_exact_checkout(repository, worker_root, "retained worker root")?;
    validate_commit_object(
        repository,
        worker_root,
        expected_worker_head,
        "retained worker HEAD",
    )?;
    if exact_ref_head(worker_root, worker_branch_ref)? != expected_worker_head {
        return Err("retained worker ref or HEAD drifted".into());
    }
    worker.worktree_identity(worker_branch_ref)
}

pub fn cleanup_retained_worktree(
    repository: &OpenedRepository,
    worker_root: &Path,
    worker_branch_ref: &str,
    expected_worker_head: &str,
    retention_proof_sha256: &str,
) -> Result<GitCleanupResult, String> {
    if worker_root.exists() {
        verify_retained_worktree(
            repository,
            worker_root,
            worker_branch_ref,
            expected_worker_head,
            retention_proof_sha256,
        )?;
        let root_text = worker_root
            .to_str()
            .ok_or_else(|| "retained worker root is not UTF-8".to_string())?;
        coordinator_git_text(&repository.root, &["worktree", "unlock", root_text])?;
        if exact_branch_ref_oid(repository, &repository.root, worker_branch_ref)?
            != expected_worker_head
        {
            return Err("retained worker ref drifted after unlock; worktree retained".into());
        }
        coordinator_git_text(
            &repository.root,
            &["worktree", "remove", "--force", root_text],
        )?;
        if worker_root.exists() {
            return Err("Git reported retained worktree removal but the exact root remains".into());
        }
    } else {
        validate_coordinator_environment()?;
        repository.validate_unchanged()?;
        schema::Sha256Digest::parse(retention_proof_sha256)?;
        validate_branch_ref(worker_branch_ref, "retained worker branch ref")?;
        validate_commit_object(
            repository,
            &repository.root,
            expected_worker_head,
            "retained worker HEAD",
        )?;
    }
    match optional_branch_ref_oid(repository, &repository.root, worker_branch_ref)? {
        Some(branch) if branch == expected_worker_head => {
            coordinator_git_text(
                &repository.root,
                &[
                    "update-ref",
                    "--no-deref",
                    "-d",
                    worker_branch_ref,
                    expected_worker_head,
                ],
            )?;
        }
        Some(_) => {
            return Err("retained worker ref drifted after worktree removal; ref retained".into());
        }
        None => {}
    }
    if optional_branch_ref_oid(repository, &repository.root, worker_branch_ref)?.is_some() {
        return Err("retained worker branch ref remains after cleanup".into());
    }
    Ok(GitCleanupResult {
        removed_worktree: worker_root.to_path_buf(),
        removed_branch_ref: worker_branch_ref.into(),
        retained_head_oid: expected_worker_head.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn command(cwd: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().into()
    }
    #[test]
    fn preservation_modes_and_applied_wip_are_real() {
        let root =
            std::env::temp_dir().join(format!("session-relay-s3-{}", crate::store::uuid_v4()));
        fs::create_dir(&root).unwrap();
        let repository_root = root.join("repository");
        fs::create_dir(&repository_root).unwrap();
        command(&repository_root, &["init", "-q"]);
        command(&repository_root, &["config", "user.name", "Test"]);
        command(
            &repository_root,
            &["config", "user.email", "test@example.invalid"],
        );
        fs::write(repository_root.join("tracked.txt"), "base\n").unwrap();
        command(&repository_root, &["add", "tracked.txt"]);
        command(&repository_root, &["commit", "-qm", "base"]);
        let base = command(&repository_root, &["rev-parse", "HEAD"]);
        fs::write(repository_root.join("tracked.txt"), "changed\n").unwrap();
        fs::write(repository_root.join("untracked.bin"), [0, 1, 2, 3]).unwrap();
        let repository = OpenedRepository::open(&repository_root).unwrap();
        let original = source_snapshot(&repository).unwrap();
        let preserve_root = root.join("preserved");
        let data_root = root.join("data");
        authority::ensure_private_directory(&data_root, unsafe { libc::geteuid() }).unwrap();
        for (index, mode) in ["commit", "artifact"].into_iter().enumerate() {
            let request_id = crate::store::uuid_v4();
            let request = PreserveRequestV1 {
                request_id: request_id.clone(),
                repository_path: repository.root.to_string_lossy().into_owned(),
                base_commit: base.clone(),
                mode: mode.into(),
                label: "smoke".into(),
                created_at: "2026-07-22T00:00:00.000Z".into(),
            };
            let result = preserve(&repository, &request, &"a".repeat(64), &preserve_root).unwrap();
            assert_eq!(source_snapshot(&repository).unwrap(), original);
            let worktree = data_root.join(format!("worker-{index}"));
            let branch_ref = format!("refs/heads/docks/{request_id}/smoke");
            provision_worktree(
                &repository,
                &worktree,
                &branch_ref,
                &request_id,
                "smoke",
                &base,
            )
            .unwrap();
            let applied = apply_wip(
                &repository,
                &worktree,
                &branch_ref,
                &base,
                &result.receipt,
                "2026-07-22T00:00:00.000Z",
            )
            .unwrap();
            assert_eq!(command(&worktree, &["rev-parse", "HEAD"]), applied);
            assert_eq!(
                fs::read_to_string(worktree.join("tracked.txt")).unwrap(),
                "changed\n"
            );
            assert_eq!(
                fs::read(worktree.join("untracked.bin")).unwrap(),
                [0, 1, 2, 3]
            );
            assert_eq!(command(&worktree, &["rev-parse", "HEAD^"]), base);
            command(
                &repository.root,
                &["worktree", "unlock", worktree.to_str().unwrap()],
            );
            command(
                &repository.root,
                &["worktree", "remove", worktree.to_str().unwrap()],
            );
        }
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn changed_path_authorization_is_exact_case() {
        let file_claim = PathClaimRequestV1 {
            path: "src/Foo.rs".into(),
            path_type: "file".into(),
            mode: "exclusive".into(),
        };
        let directory_claim = PathClaimRequestV1 {
            path: "Assets".into(),
            path_type: "directory".into(),
            mode: "exclusive".into(),
        };
        assert!(
            validate_changed_paths(
                &[NameStatusChange {
                    status: "M".into(),
                    source: None,
                    destination: "src/Foo.rs".into()
                }],
                &[file_claim.clone()]
            )
            .is_ok()
        );
        assert!(
            validate_changed_paths(
                &[NameStatusChange {
                    status: "M".into(),
                    source: None,
                    destination: "src/foo.rs".into()
                }],
                &[file_claim]
            )
            .is_err()
        );
        assert!(
            validate_changed_paths(
                &[NameStatusChange {
                    status: "M".into(),
                    source: None,
                    destination: "Assets/logo.svg".into()
                }],
                &[directory_claim.clone()]
            )
            .is_ok()
        );
        assert!(
            validate_changed_paths(
                &[NameStatusChange {
                    status: "M".into(),
                    source: None,
                    destination: "assets/logo.svg".into()
                }],
                &[directory_claim]
            )
            .is_err()
        );
    }
    #[test]
    fn artifact_bytes_are_receipt_bound_and_empty_diff_is_supported() {
        let root = std::env::temp_dir().join(format!(
            "session-relay-artifact-{}",
            crate::store::uuid_v4()
        ));
        fs::create_dir(&root).unwrap();
        let repository_root = root.join("repository");
        fs::create_dir(&repository_root).unwrap();
        command(&repository_root, &["init", "-q"]);
        command(&repository_root, &["config", "user.name", "Test"]);
        command(
            &repository_root,
            &["config", "user.email", "test@example.invalid"],
        );
        fs::write(repository_root.join("tracked.txt"), "base\n").unwrap();
        command(&repository_root, &["add", "tracked.txt"]);
        command(&repository_root, &["commit", "-qm", "base"]);
        let base = command(&repository_root, &["rev-parse", "HEAD"]);
        fs::write(repository_root.join("tracked.txt"), "changed\n").unwrap();
        fs::write(repository_root.join("only-untracked.txt"), "payload\n").unwrap();
        let repository = OpenedRepository::open(&repository_root).unwrap();
        let preserve_root = root.join("preserved");
        let data_root = root.join("data");
        authority::ensure_private_directory(&data_root, unsafe { libc::geteuid() }).unwrap();
        let make_request = || {
            let request_id = crate::store::uuid_v4();
            PreserveRequestV1 {
                request_id,
                repository_path: repository.root.to_string_lossy().into_owned(),
                base_commit: base.clone(),
                mode: "artifact".into(),
                label: "smoke".into(),
                created_at: "2026-07-22T00:00:00.000Z".into(),
            }
        };
        let tampered_request = make_request();
        let tampered = preserve(
            &repository,
            &tampered_request,
            &"a".repeat(64),
            &preserve_root,
        )
        .unwrap();
        let WipPayloadV1::Artifact {
            binary_diff,
            untracked_inventory,
            untracked_archive,
            ..
        } = &tampered.receipt.payload
        else {
            panic!("artifact payload")
        };
        for (index, path) in [binary_diff, untracked_inventory, untracked_archive]
            .into_iter()
            .enumerate()
        {
            let original = fs::read(path).unwrap();
            fs::write(path, b"different valid-looking bytes\n").unwrap();
            let session_id = crate::store::uuid_v4();
            let tampered_worktree = data_root.join(format!("tampered-{index}"));
            let tampered_branch = format!("refs/heads/docks/{session_id}/smoke");
            provision_worktree(
                &repository,
                &tampered_worktree,
                &tampered_branch,
                &session_id,
                "smoke",
                &base,
            )
            .unwrap();
            assert!(
                apply_wip(
                    &repository,
                    &tampered_worktree,
                    &tampered_branch,
                    &base,
                    &tampered.receipt,
                    "2026-07-22T00:00:00.000Z"
                )
                .is_err()
            );
            assert_eq!(command(&tampered_worktree, &["rev-parse", "HEAD"]), base);
            assert!(!tampered_worktree.join("only-untracked.txt").exists());
            command(
                &repository.root,
                &["worktree", "unlock", tampered_worktree.to_str().unwrap()],
            );
            command(
                &repository.root,
                &["worktree", "remove", tampered_worktree.to_str().unwrap()],
            );
            fs::write(path, original).unwrap();
        }
        let clean_request = make_request();
        let clean = preserve(&repository, &clean_request, &"b".repeat(64), &preserve_root).unwrap();
        let clean_worktree = data_root.join("clean");
        let clean_branch = format!("refs/heads/docks/{}/smoke", clean_request.request_id);
        provision_worktree(
            &repository,
            &clean_worktree,
            &clean_branch,
            &clean_request.request_id,
            "smoke",
            &base,
        )
        .unwrap();
        let WipPayloadV1::Artifact {
            binary_diff,
            untracked_inventory,
            untracked_archive,
            ..
        } = &clean.receipt.payload
        else {
            panic!("artifact payload")
        };
        let replaced = [
            binary_diff.clone(),
            untracked_inventory.clone(),
            untracked_archive.clone(),
        ];
        set_after_artifact_verify(move || {
            for path in replaced {
                let verified = PathBuf::from(&path)
                    .with_file_name(format!(".verified-{}", crate::store::uuid_v4()));
                fs::rename(&path, verified).unwrap();
                fs::write(&path, b"replacement bytes").unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            }
        });
        let applied = apply_wip(
            &repository,
            &clean_worktree,
            &clean_branch,
            &base,
            &clean.receipt,
            "2026-07-22T00:00:00.000Z",
        )
        .unwrap();
        assert_ne!(applied, base);
        assert_eq!(
            fs::read_to_string(clean_worktree.join("tracked.txt")).unwrap(),
            "changed\n"
        );
        assert_eq!(
            fs::read_to_string(clean_worktree.join("only-untracked.txt")).unwrap(),
            "payload\n"
        );
        command(
            &repository.root,
            &["worktree", "unlock", clean_worktree.to_str().unwrap()],
        );
        command(
            &repository.root,
            &["worktree", "remove", clean_worktree.to_str().unwrap()],
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn empty_tracked_diff_artifact_is_supported() {
        let root = std::env::temp_dir().join(format!(
            "session-relay-empty-artifact-{}",
            crate::store::uuid_v4()
        ));
        let repository_root = root.join("repository");
        fs::create_dir_all(&repository_root).unwrap();
        command(&repository_root, &["init", "-q"]);
        command(&repository_root, &["config", "user.name", "Test"]);
        command(
            &repository_root,
            &["config", "user.email", "test@example.invalid"],
        );
        fs::write(repository_root.join("tracked.txt"), "base\n").unwrap();
        command(&repository_root, &["add", "tracked.txt"]);
        command(&repository_root, &["commit", "-qm", "base"]);
        let base = command(&repository_root, &["rev-parse", "HEAD"]);
        fs::write(repository_root.join("untracked.txt"), "payload\n").unwrap();
        let repository = OpenedRepository::open(&repository_root).unwrap();
        let request_id = crate::store::uuid_v4();
        let request = PreserveRequestV1 {
            request_id: request_id.clone(),
            repository_path: repository.root.to_string_lossy().into_owned(),
            base_commit: base.clone(),
            mode: "artifact".into(),
            label: "empty".into(),
            created_at: "2026-07-22T00:00:00.000Z".into(),
        };
        let result = preserve(
            &repository,
            &request,
            &"c".repeat(64),
            &root.join("preserved"),
        )
        .unwrap();
        let WipPayloadV1::Artifact { binary_diff, .. } = &result.receipt.payload else {
            panic!("artifact payload")
        };
        assert!(fs::read(binary_diff).unwrap().is_empty());
        let data = root.join("data");
        authority::ensure_private_directory(&data, unsafe { libc::geteuid() }).unwrap();
        let worker = data.join("worker");
        let branch = format!("refs/heads/docks/{request_id}/empty");
        provision_worktree(&repository, &worker, &branch, &request_id, "empty", &base).unwrap();
        apply_wip(
            &repository,
            &worker,
            &branch,
            &base,
            &result.receipt,
            "2026-07-22T00:00:00.000Z",
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(worker.join("untracked.txt")).unwrap(),
            "payload\n"
        );
        command(
            &repository.root,
            &["worktree", "unlock", worker.to_str().unwrap()],
        );
        command(
            &repository.root,
            &["worktree", "remove", worker.to_str().unwrap()],
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn artifact_preservation_rejects_unsafe_untracked_types_omitted_by_git() {
        let root = std::env::temp_dir().join(format!(
            "session-relay-unsafe-artifact-{}",
            crate::store::uuid_v4()
        ));
        let repository_root = root.join("repository");
        fs::create_dir_all(&repository_root).unwrap();
        command(&repository_root, &["init", "-q"]);
        command(&repository_root, &["config", "user.name", "Test"]);
        command(
            &repository_root,
            &["config", "user.email", "test@example.invalid"],
        );
        fs::write(repository_root.join("tracked.txt"), "base\n").unwrap();
        command(&repository_root, &["add", "tracked.txt"]);
        command(&repository_root, &["commit", "-qm", "base"]);
        let repository = OpenedRepository::open(&repository_root).unwrap();
        let preserve_root = root.join("preserved");
        let request = |label: &str| PreserveRequestV1 {
            request_id: crate::store::uuid_v4(),
            repository_path: repository.root.to_string_lossy().into_owned(),
            base_commit: command(&repository_root, &["rev-parse", "HEAD"]),
            mode: "artifact".into(),
            label: label.into(),
            created_at: "2026-07-22T00:00:00.000Z".into(),
        };
        let fifo = repository_root.join("unsafe.fifo");
        let fifo_c =
            std::ffi::CString::new(std::os::unix::ffi::OsStrExt::as_bytes(fifo.as_os_str()))
                .unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);
        assert!(
            preserve(
                &repository,
                &request("fifo"),
                &"a".repeat(64),
                &preserve_root
            )
            .unwrap_err()
            .contains("unsupported type")
        );
        fs::remove_file(&fifo).unwrap();
        let socket = repository_root.join("unsafe.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        assert!(
            preserve(
                &repository,
                &request("socket"),
                &"b".repeat(64),
                &preserve_root
            )
            .unwrap_err()
            .contains("unsupported type")
        );
        drop(listener);
        fs::remove_file(&socket).unwrap();
        let device = repository_root.join("unsafe.device");
        let device_c =
            std::ffi::CString::new(std::os::unix::ffi::OsStrExt::as_bytes(device.as_os_str()))
                .unwrap();
        if unsafe {
            libc::mknod(
                device_c.as_ptr(),
                libc::S_IFCHR | 0o600,
                libc::makedev(1, 7),
            )
        } == 0
        {
            assert!(
                preserve(
                    &repository,
                    &request("device"),
                    &"c".repeat(64),
                    &preserve_root
                )
                .unwrap_err()
                .contains("unsupported type")
            );
            fs::remove_file(&device).unwrap();
        }
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn artifact_archive_uses_opened_member_when_source_path_is_replaced() {
        let root = std::env::temp_dir().join(format!(
            "session-relay-artifact-race-{}",
            crate::store::uuid_v4()
        ));
        let repository_root = root.join("repository");
        fs::create_dir_all(&repository_root).unwrap();
        command(&repository_root, &["init", "-q"]);
        command(&repository_root, &["config", "user.name", "Test"]);
        command(
            &repository_root,
            &["config", "user.email", "test@example.invalid"],
        );
        fs::write(repository_root.join("tracked.txt"), "base\n").unwrap();
        command(&repository_root, &["add", "tracked.txt"]);
        command(&repository_root, &["commit", "-qm", "base"]);
        let base = command(&repository_root, &["rev-parse", "HEAD"]);
        let member = repository_root.join("payload.txt");
        fs::write(&member, "trusted\n").unwrap();
        let moved = root.join("moved-original");
        let replaced = member.clone();
        set_before_pax_archive(move || {
            fs::rename(&replaced, &moved).unwrap();
            fs::write(&replaced, "replacement\n").unwrap();
        });
        let repository = OpenedRepository::open(&repository_root).unwrap();
        let request = PreserveRequestV1 {
            request_id: crate::store::uuid_v4(),
            repository_path: repository.root.to_string_lossy().into_owned(),
            base_commit: base,
            mode: "artifact".into(),
            label: "race".into(),
            created_at: "2026-07-22T00:00:00.000Z".into(),
        };
        let result = preserve(
            &repository,
            &request,
            &"d".repeat(64),
            &root.join("preserved"),
        )
        .unwrap();
        let WipPayloadV1::Artifact {
            untracked_archive, ..
        } = &result.receipt.payload
        else {
            panic!("artifact payload")
        };
        let extracted = root.join("extracted");
        fs::create_dir(&extracted).unwrap();
        let output = Command::new("tar")
            .args(["-xf", untracked_archive])
            .current_dir(&extracted)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "tar extraction: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            fs::read_to_string(extracted.join("payload.txt")).unwrap(),
            "trusted\n"
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn opened_repository_git_stays_on_held_private_git_after_dot_git_swap() {
        let root = std::env::temp_dir().join(format!(
            "session-relay-bound-git-{}",
            crate::store::uuid_v4()
        ));
        let repository_root = root.join("repository");
        let worker = root.join("worker");
        fs::create_dir_all(&repository_root).unwrap();
        command(&repository_root, &["init", "-q"]);
        command(&repository_root, &["config", "user.name", "Test"]);
        command(
            &repository_root,
            &["config", "user.email", "test@example.invalid"],
        );
        fs::write(repository_root.join("tracked.txt"), "base\n").unwrap();
        command(&repository_root, &["add", "tracked.txt"]);
        command(&repository_root, &["commit", "-qm", "base"]);
        let branch = "docks/test/bound";
        command(
            &repository_root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                branch,
                worker.to_str().unwrap(),
            ],
        );
        let expected = format!("refs/heads/{branch}");
        let repository = OpenedRepository::open(&worker).unwrap();
        fs::rename(worker.join(".git"), worker.join(".git.verified")).unwrap();
        command(&worker, &["init", "-q"]);
        assert_ne!(
            run_git_text(&worker, &["symbolic-ref", "-q", "HEAD"]).unwrap(),
            expected
        );
        assert_eq!(
            repository
                .run_git_text(&["symbolic-ref", "-q", "HEAD"])
                .unwrap(),
            expected,
            "Git operation followed a replacement .git entry instead of the held private Git identity"
        );
        fs::remove_dir_all(worker.join(".git")).unwrap();
        fs::rename(worker.join(".git.verified"), worker.join(".git")).unwrap();
        command(
            &repository_root,
            &["worktree", "remove", worker.to_str().unwrap()],
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn ordered_integration_imports_every_commit_oldest_first() {
        let root = std::env::temp_dir().join(format!(
            "session-relay-integrate-{}",
            crate::store::uuid_v4()
        ));
        fs::create_dir(&root).unwrap();
        let integration = root.join("integration");
        let worker = root.join("worker");
        fs::create_dir(&integration).unwrap();
        command(&integration, &["init", "-q"]);
        command(&integration, &["config", "user.name", "Test"]);
        command(
            &integration,
            &["config", "user.email", "test@example.invalid"],
        );
        fs::write(integration.join("tracked.txt"), "base\n").unwrap();
        command(&integration, &["add", "tracked.txt"]);
        command(&integration, &["commit", "-qm", "base"]);
        let base = command(&integration, &["rev-parse", "HEAD"]);
        let integration_branch = command(&integration, &["symbolic-ref", "HEAD"]);
        let session = crate::store::uuid_v4();
        let worker_branch = format!("refs/heads/docks/{session}/ordered");
        command(
            &integration,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                worker_branch.strip_prefix("refs/heads/").unwrap(),
                worker.to_str().unwrap(),
                &base,
            ],
        );
        command(&worker, &["commit", "--allow-empty", "-qm", "applied WIP"]);
        let applied = command(&worker, &["rev-parse", "HEAD"]);
        fs::write(worker.join("tracked.txt"), "worker\n").unwrap();
        command(&worker, &["add", "tracked.txt"]);
        command(&worker, &["commit", "-qm", "worker"]);
        let produced = command(&worker, &["rev-parse", "HEAD"]);
        let repository = OpenedRepository::open(&integration).unwrap();
        let request = OrderedIntegrationRequest {
            integration_root: integration.clone(),
            integration_branch_ref: integration_branch,
            expected_integration_head: base.clone(),
            worker_root: worker.clone(),
            worker_branch_ref: worker_branch.clone(),
            expected_worker_head: produced.clone(),
            base_commit: base.clone(),
            produced_commits: vec![
                OrderedProducedCommit {
                    oid: applied.clone(),
                    parent_oid: base.clone(),
                    source: "applied_wip".into(),
                },
                OrderedProducedCommit {
                    oid: produced.clone(),
                    parent_oid: applied,
                    source: "worker".into(),
                },
            ],
        };
        let integration_index_before = exact_index_file(&integration).unwrap();
        let worker_index_before = exact_index_file(&worker).unwrap();
        let rejected = reject_ordered(&repository, &request).unwrap();
        assert_eq!(rejected.outcome, CoordinatorIntegrationOutcome::Rejected);
        assert_eq!(rejected.pre_head, base);
        assert_eq!(rejected.post_head, base);
        assert!(rejected.output_oids.is_empty());
        assert!(rejected.conflict_paths.is_empty());
        assert_eq!(command(&integration, &["rev-parse", "HEAD"]), base);
        assert_eq!(
            exact_index_file(&integration).unwrap(),
            integration_index_before
        );
        assert_eq!(exact_index_file(&worker).unwrap(), worker_index_before);
        let result = integrate_ordered(&repository, &request).unwrap();
        assert_eq!(result.outcome, CoordinatorIntegrationOutcome::Integrated);
        assert_eq!(result.pre_head, base);
        assert_eq!(result.output_oids.len(), 2);
        assert_eq!(result.post_head, result.output_oids[1]);
        assert_eq!(
            command(&integration, &["rev-parse", "HEAD"]),
            result.post_head
        );
        assert_eq!(
            command(
                &integration,
                &["rev-parse", &format!("{}^", result.output_oids[1])]
            ),
            result.output_oids[0]
        );
        assert_eq!(command(&worker, &["rev-parse", "HEAD"]), produced);
        command(
            &integration,
            &[
                "worktree",
                "lock",
                "--reason",
                "session-relay:test",
                worker.to_str().unwrap(),
            ],
        );
        let cleanup_proof = GitCleanupProof::Integrated(result);
        let cleanup = cleanup_ordered_worktree(&repository, &request, &cleanup_proof).unwrap();
        assert_eq!(cleanup.removed_worktree, worker);
        assert!(!cleanup.removed_worktree.exists());
        assert!(
            coordinator_git_output(
                &integration,
                &["show-ref", "--verify", "--quiet", &worker_branch]
            )
            .unwrap()
            .status
            .code()
                == Some(1)
        );
        assert_eq!(
            cleanup_ordered_worktree(&repository, &request, &cleanup_proof).unwrap(),
            cleanup,
            "cleanup replay must prove the already-removed exact worktree and ref"
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn retained_cleanup_replay_proves_already_removed_exact_identity() {
        let root = std::env::temp_dir().join(format!(
            "session-relay-retained-cleanup-{}",
            crate::store::uuid_v4()
        ));
        let repository_root = root.join("repository");
        let worker = root.join("worker");
        fs::create_dir_all(&repository_root).unwrap();
        command(&repository_root, &["init", "-q"]);
        command(&repository_root, &["config", "user.name", "Test"]);
        command(
            &repository_root,
            &["config", "user.email", "test@example.invalid"],
        );
        fs::write(repository_root.join("tracked.txt"), "base\n").unwrap();
        command(&repository_root, &["add", "tracked.txt"]);
        command(&repository_root, &["commit", "-qm", "base"]);
        let head = command(&repository_root, &["rev-parse", "HEAD"]);
        let branch = format!("refs/heads/docks/{}/retained", crate::store::uuid_v4());
        command(
            &repository_root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                branch.strip_prefix("refs/heads/").unwrap(),
                worker.to_str().unwrap(),
                &head,
            ],
        );
        fs::write(worker.join("dirty.txt"), "retained user bytes\n").unwrap();
        command(
            &repository_root,
            &[
                "worktree",
                "lock",
                "--reason",
                "session-relay:test",
                worker.to_str().unwrap(),
            ],
        );
        let repository = OpenedRepository::open(&repository_root).unwrap();
        let retention_sha256 = "a".repeat(64);
        let first =
            cleanup_retained_worktree(&repository, &worker, &branch, &head, &retention_sha256)
                .unwrap();
        assert!(!worker.exists());
        assert_eq!(
            cleanup_retained_worktree(&repository, &worker, &branch, &head, &retention_sha256)
                .unwrap(),
            first,
            "retained cleanup replay must prove the already-removed exact worktree and ref"
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn integration_conflict_restores_exact_prestate_without_partial_outputs() {
        let root = std::env::temp_dir().join(format!(
            "session-relay-conflict-{}",
            crate::store::uuid_v4()
        ));
        fs::create_dir(&root).unwrap();
        let integration = root.join("integration");
        let worker = root.join("worker");
        fs::create_dir(&integration).unwrap();
        command(&integration, &["init", "-q"]);
        command(&integration, &["config", "user.name", "Test"]);
        command(
            &integration,
            &["config", "user.email", "test@example.invalid"],
        );
        fs::write(integration.join("tracked.txt"), "base\n").unwrap();
        command(&integration, &["add", "tracked.txt"]);
        command(&integration, &["commit", "-qm", "base"]);
        let base = command(&integration, &["rev-parse", "HEAD"]);
        let integration_branch = command(&integration, &["symbolic-ref", "HEAD"]);
        let session = crate::store::uuid_v4();
        let worker_branch = format!("refs/heads/docks/{session}/conflict");
        command(
            &integration,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                worker_branch.strip_prefix("refs/heads/").unwrap(),
                worker.to_str().unwrap(),
                &base,
            ],
        );
        command(&worker, &["commit", "--allow-empty", "-qm", "applied WIP"]);
        let applied = command(&worker, &["rev-parse", "HEAD"]);
        fs::write(worker.join("tracked.txt"), "worker\n").unwrap();
        command(&worker, &["add", "tracked.txt"]);
        command(&worker, &["commit", "-qm", "worker"]);
        let produced = command(&worker, &["rev-parse", "HEAD"]);
        fs::write(integration.join("tracked.txt"), "integration\n").unwrap();
        command(&integration, &["add", "tracked.txt"]);
        command(&integration, &["commit", "-qm", "integration"]);
        let pre_head = command(&integration, &["rev-parse", "HEAD"]);
        let repository = OpenedRepository::open(&integration).unwrap();
        let pre =
            pristine_git_state(&repository, &integration, &integration_branch, &pre_head).unwrap();
        let request = OrderedIntegrationRequest {
            integration_root: integration.clone(),
            integration_branch_ref: integration_branch.clone(),
            expected_integration_head: pre_head.clone(),
            worker_root: worker.clone(),
            worker_branch_ref: worker_branch.clone(),
            expected_worker_head: produced.clone(),
            base_commit: base.clone(),
            produced_commits: vec![
                OrderedProducedCommit {
                    oid: applied.clone(),
                    parent_oid: base,
                    source: "applied_wip".into(),
                },
                OrderedProducedCommit {
                    oid: produced.clone(),
                    parent_oid: applied,
                    source: "worker".into(),
                },
            ],
        };
        let result = integrate_ordered(&repository, &request).unwrap();
        assert_eq!(
            result.outcome,
            CoordinatorIntegrationOutcome::NeedsUserAction
        );
        assert_eq!(result.pre_head, pre_head);
        assert_eq!(result.post_head, pre_head);
        assert!(result.output_oids.is_empty());
        assert_eq!(result.conflict_paths, vec!["tracked.txt"]);
        assert_eq!(
            sha256::hex_digest(&exact_index_file(&integration).unwrap()),
            pre.index_file_sha256
        );
        let mut restored =
            pristine_git_state(&repository, &integration, &integration_branch, &pre_head).unwrap();
        restored.index_file_sha256 = pre.index_file_sha256.clone();
        assert_eq!(restored, pre);
        assert_eq!(command(&worker, &["rev-parse", "HEAD"]), produced);
        command(
            &integration,
            &["worktree", "remove", worker.to_str().unwrap()],
        );
        command(
            &integration,
            &[
                "branch",
                "-D",
                worker_branch.strip_prefix("refs/heads/").unwrap(),
            ],
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn queued_integration_recovers_a_partial_exact_prefix_before_retry() {
        let root = std::env::temp_dir().join(format!(
            "session-relay-integration-recovery-{}",
            crate::store::uuid_v4()
        ));
        fs::create_dir(&root).unwrap();
        let integration = root.join("integration");
        let worker = root.join("worker");
        fs::create_dir(&integration).unwrap();
        command(&integration, &["init", "-q"]);
        command(&integration, &["config", "user.name", "Test"]);
        command(
            &integration,
            &["config", "user.email", "test@example.invalid"],
        );
        fs::write(integration.join("first.txt"), "base\n").unwrap();
        fs::write(integration.join("second.txt"), "base\n").unwrap();
        command(&integration, &["add", "first.txt", "second.txt"]);
        command(&integration, &["commit", "-qm", "base"]);
        let base = command(&integration, &["rev-parse", "HEAD"]);
        let integration_branch = command(&integration, &["symbolic-ref", "HEAD"]);
        let session = crate::store::uuid_v4();
        let worker_branch = format!("refs/heads/docks/{session}/recovery");
        command(
            &integration,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                worker_branch.strip_prefix("refs/heads/").unwrap(),
                worker.to_str().unwrap(),
                &base,
            ],
        );
        fs::write(worker.join("first.txt"), "first\n").unwrap();
        command(&worker, &["add", "first.txt"]);
        command(&worker, &["commit", "-qm", "applied WIP"]);
        let first = command(&worker, &["rev-parse", "HEAD"]);
        fs::write(worker.join("second.txt"), "second\n").unwrap();
        command(&worker, &["add", "second.txt"]);
        command(&worker, &["commit", "-qm", "worker"]);
        let second = command(&worker, &["rev-parse", "HEAD"]);
        let repository = OpenedRepository::open(&integration).unwrap();
        let request = OrderedIntegrationRequest {
            integration_root: integration.clone(),
            integration_branch_ref: integration_branch.clone(),
            expected_integration_head: base.clone(),
            worker_root: worker.clone(),
            worker_branch_ref: worker_branch.clone(),
            expected_worker_head: second.clone(),
            base_commit: base.clone(),
            produced_commits: vec![
                OrderedProducedCommit {
                    oid: first.clone(),
                    parent_oid: base.clone(),
                    source: "applied_wip".into(),
                },
                OrderedProducedCommit {
                    oid: second,
                    parent_oid: first,
                    source: "worker".into(),
                },
            ],
        };
        let prestate = capture_ordered_integration_prestate(&repository, &request).unwrap();
        let durable = std::cell::RefCell::new(Vec::new());
        let error = integrate_ordered_with_step(&repository, &request, |outputs| {
            durable.replace(outputs.to_vec());
            Err("injected progress publication fault".into())
        })
        .unwrap_err();
        assert_eq!(error, "injected progress publication fault");
        let partial = durable.into_inner();
        assert_eq!(partial.len(), 1);
        assert_eq!(command(&integration, &["rev-parse", "HEAD"]), partial[0]);
        rollback_queued_integration(&repository, &request, &prestate, &partial, |_| Ok(()))
            .unwrap();
        assert_eq!(command(&integration, &["rev-parse", "HEAD"]), base);
        assert_eq!(
            exact_index_file(&integration).unwrap(),
            prestate.integration_index_bytes
        );
        let result = integrate_ordered(&repository, &request).unwrap();
        assert_eq!(result.output_oids.len(), 2);
        assert_eq!(
            command(&integration, &["rev-parse", "HEAD"]),
            result.post_head
        );
        command(
            &integration,
            &["worktree", "remove", worker.to_str().unwrap()],
        );
        command(
            &integration,
            &[
                "branch",
                "-D",
                worker_branch.strip_prefix("refs/heads/").unwrap(),
            ],
        );
        fs::remove_dir_all(root).unwrap();
    }
}
