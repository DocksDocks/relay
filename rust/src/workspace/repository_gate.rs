use super::authority::{self, AuthorityRoots};
use super::schema::{self, ClosedJcs, JcsValue, RepositoryIdentityV1};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::marker::PhantomData;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

pub const GATE_PROTOCOL: &str = "RepositoryGateV1";
pub const GATE_TIMEOUT: Duration = Duration::from_secs(3);
const MARKER_RELATIVE_C: &[u8] = b"docks/workspace-admission-v1.json\0";

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ShortLockRank {
    IntegrationQueue = 10,
    RepositoryGate = 20,
    RelayStore = 30,
    AuthorityExclusion = 35,
    WorkspaceJournal = 40,
}

thread_local! {
    static HELD_SHORT_LOCKS: RefCell<Vec<ShortLockRank>> = const { RefCell::new(Vec::new()) };
}

pub(crate) fn assert_no_short_locks(operation: &str) -> Result<(), String> {
    HELD_SHORT_LOCKS.with(|held| {
        let held = held.borrow();
        if held.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "{operation} cannot run while short lock {:?} is held",
                held.last().unwrap()
            ))
        }
    })
}

pub(crate) struct ShortLockToken {
    rank: ShortLockRank,
    _not_send: PhantomData<Rc<()>>,
}

impl Drop for ShortLockToken {
    fn drop(&mut self) {
        HELD_SHORT_LOCKS.with(|held| {
            let mut held = held.borrow_mut();
            if let Some(index) = held.iter().rposition(|rank| *rank == self.rank) {
                held.remove(index);
            }
        });
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LockFileIdentity {
    pub device: u64,
    pub inode: u64,
}

fn enter_short_lock(rank: ShortLockRank) -> Result<ShortLockToken, String> {
    HELD_SHORT_LOCKS.with(|held| {
        let mut held = held.borrow_mut();
        if let Some(current) = held.iter().max().copied() {
            if current >= rank {
                return Err(format!(
                    "short-lock rank reversal: cannot acquire {rank:?} while {current:?} is held"
                ));
            }
        }
        held.push(rank);
        Ok(ShortLockToken {
            rank,
            _not_send: PhantomData,
        })
    })
}

pub fn with_relay_store_rank<T>(
    operation: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    let _rank = enter_short_lock(ShortLockRank::RelayStore)?;
    operation()
}

pub(crate) fn acquire_ranked_lock(
    path: &Path,
    euid: u32,
    label: &str,
    rank: ShortLockRank,
) -> Result<(File, LockFileIdentity, ShortLockToken), String> {
    let token = enter_short_lock(rank)?;
    let (file, created) = match OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => (file, true),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
                .open(path)
                .map_err(|open_error| {
                    format!("open existing {label} {}: {open_error}", path.display())
                })?;
            (file, false)
        }
        Err(error) => return Err(format!("create {label} {}: {error}", path.display())),
    };
    let metadata = file
        .metadata()
        .map_err(|error| format!("fstat {label}: {error}"))?;
    if !metadata.is_file()
        || metadata.uid() != euid
        || metadata.nlink() != 1
        || metadata.mode() & 0o777 != 0o600
    {
        return Err(format!("{label} has unsafe owner/type/link/mode"));
    }
    let identity = LockFileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    };
    if created {
        file.sync_all()
            .map_err(|error| format!("fsync new {label}: {error}"))?;
        let parent = path
            .parent()
            .ok_or_else(|| format!("{label} path has no parent"))?;
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_DIRECTORY)
            .open(parent)
            .map_err(|error| format!("open {label} parent for fsync: {error}"))?;
        directory
            .sync_all()
            .map_err(|error| format!("fsync {label} parent: {error}"))?;
    }
    let deadline = Instant::now() + GATE_TIMEOUT;
    loop {
        if Instant::now() >= deadline {
            return Err(format!(
                "{label} contention exceeded three seconds; no mutation performed"
            ));
        }
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result == 0 {
            break;
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINTR)
            && error.raw_os_error() != Some(libc::EAGAIN)
            && error.raw_os_error() != Some(libc::EWOULDBLOCK)
        {
            return Err(format!("lock {label}: {error}"));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if error.raw_os_error() != Some(libc::EINTR) {
            std::thread::sleep(remaining.min(Duration::from_millis(10)));
        }
    }
    revalidate_ranked_lock(&file, path, identity, euid, label)?;
    Ok((file, identity, token))
}

pub(crate) fn revalidate_ranked_lock(
    file: &File,
    path: &Path,
    expected: LockFileIdentity,
    euid: u32,
    label: &str,
) -> Result<(), String> {
    let opened = file
        .metadata()
        .map_err(|error| format!("fstat {label}: {error}"))?;
    let named = fs::symlink_metadata(path)
        .map_err(|error| format!("revalidate {label} path {}: {error}", path.display()))?;
    let safe = |metadata: &fs::Metadata| {
        metadata.is_file()
            && metadata.uid() == euid
            && metadata.nlink() == 1
            && metadata.mode() & 0o777 == 0o600
            && metadata.dev() == expected.device
            && metadata.ino() == expected.inode
    };
    if !safe(&opened) || !safe(&named) {
        return Err(format!("{label} inode identity changed while locked"));
    }
    Ok(())
}

fn open_private_marker_directory(
    repository: &RepositoryIdentityV1,
    euid: u32,
) -> Result<(File, PathBuf), String> {
    let common_path = Path::new(&repository.common_dir_realpath);
    let common = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(common_path)
        .map_err(|e| format!("securely open Git common directory for marker: {e}"))?;
    let common_metadata = common
        .metadata()
        .map_err(|e| format!("fstat Git common directory for marker: {e}"))?;
    let expected_dev = repository
        .common_dir_dev
        .parse::<u64>()
        .map_err(|_| "repository common-dir device is not decimal".to_string())?;
    let expected_ino = repository
        .common_dir_ino
        .parse::<u64>()
        .map_err(|_| "repository common-dir inode is not decimal".to_string())?;
    if !common_metadata.is_dir()
        || common_metadata.uid() != euid
        || common_metadata.dev() != expected_dev
        || common_metadata.ino() != expected_ino
    {
        return Err("Git common directory identity changed before marker publication".into());
    }
    let name = std::ffi::CString::new("docks").unwrap();
    let created = unsafe { libc::mkdirat(common.as_raw_fd(), name.as_ptr(), 0o700) };
    if created != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(format!("create managed marker directory: {error}"));
        }
    }
    let fd = unsafe {
        libc::openat(
            common.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_DIRECTORY,
            0,
        )
    };
    if fd < 0 {
        return Err(format!(
            "securely open managed marker directory: {}",
            std::io::Error::last_os_error()
        ));
    }
    let directory = unsafe { File::from_raw_fd(fd) };
    let metadata = directory
        .metadata()
        .map_err(|e| format!("fstat managed marker directory: {e}"))?;
    if !metadata.is_dir() || metadata.uid() != euid || metadata.mode() & 0o777 != 0o700 {
        return Err(
            "managed marker directory is not an EUID-owned mode-0700 real directory".into(),
        );
    }
    if created == 0 {
        common.sync_all().map_err(|e| {
            format!("fsync Git common directory after marker directory creation: {e}")
        })?
    }
    Ok((directory, common_path.join("docks")))
}

fn open_marker_at(
    directory: &File,
    flags: libc::c_int,
    mode: libc::c_uint,
) -> Result<File, std::io::Error> {
    let name = std::ffi::CString::new("workspace-admission-v1.json").unwrap();
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            mode,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn read_marker_at(directory: &File, euid: u32) -> Result<Option<Vec<u8>>, String> {
    let mut file = match open_marker_at(directory, libc::O_RDONLY, 0) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("securely open managed marker: {error}")),
    };
    let metadata = file
        .metadata()
        .map_err(|e| format!("fstat managed marker: {e}"))?;
    if !metadata.is_file()
        || metadata.uid() != euid
        || metadata.nlink() != 1
        || metadata.mode() & 0o777 != 0o600
    {
        return Err(
            "managed marker is not an EUID-owned single-link mode-0600 regular file".into(),
        );
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|e| format!("read managed marker: {e}"))?;
    Ok(Some(bytes))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryGateIdentity {
    pub path: PathBuf,
    pub repository_id: String,
    pub lock_file: LockFileIdentity,
}

pub struct RepositoryGate {
    file: File,
    path: PathBuf,
    repository_id: String,
    lock_file: LockFileIdentity,
    _rank: ShortLockToken,
}
impl RepositoryGate {
    pub fn acquire(
        roots: &AuthorityRoots,
        repository: &RepositoryIdentityV1,
    ) -> Result<Self, String> {
        if repository.euid != roots.euid.to_string() {
            return Err("repository identity EUID differs from authority root".into());
        }
        let gates = roots.authority.join("repository-gates");
        authority::ensure_private_directory(&gates, roots.euid)?;
        let path = gates.join(format!("{}.lock", repository.repository_id));
        let (file, lock_file, rank) = acquire_ranked_lock(
            &path,
            roots.euid,
            "RepositoryGate",
            ShortLockRank::RepositoryGate,
        )?;
        Ok(Self {
            file,
            path,
            repository_id: repository.repository_id.clone(),
            lock_file,
            _rank: rank,
        })
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn repository_id(&self) -> &str {
        &self.repository_id
    }
    pub fn as_raw_fd(&self) -> i32 {
        self.file.as_raw_fd()
    }
    pub fn identity(&self) -> RepositoryGateIdentity {
        RepositoryGateIdentity {
            path: self.path.clone(),
            repository_id: self.repository_id.clone(),
            lock_file: self.lock_file,
        }
    }
    pub fn revalidate_lock(&self, roots: &AuthorityRoots) -> Result<(), String> {
        revalidate_ranked_lock(
            &self.file,
            &self.path,
            self.lock_file,
            roots.euid,
            "RepositoryGate",
        )
    }
    fn revalidated_common_dir(
        &self,
        roots: &AuthorityRoots,
        repository: &RepositoryIdentityV1,
    ) -> Result<File, String> {
        if roots.euid.to_string() != repository.euid
            || self.repository_id != repository.repository_id
        {
            return Err("RepositoryGate identity differs from current repository authority".into());
        }
        revalidate_ranked_lock(
            &self.file,
            &self.path,
            self.lock_file,
            roots.euid,
            "RepositoryGate",
        )?;
        let common = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_DIRECTORY)
            .open(&repository.common_dir_realpath)
            .map_err(|error| {
                format!("reopen Git common directory for RepositoryGate revalidation: {error}")
            })?;
        let metadata = common.metadata().map_err(|error| {
            format!("fstat Git common directory for RepositoryGate revalidation: {error}")
        })?;
        let named = fs::symlink_metadata(&repository.common_dir_realpath)
            .map_err(|error| format!("revalidate Git common directory path: {error}"))?;
        let device = repository
            .common_dir_dev
            .parse::<u64>()
            .map_err(|_| "repository common-dir device is not decimal".to_string())?;
        let inode = repository
            .common_dir_ino
            .parse::<u64>()
            .map_err(|_| "repository common-dir inode is not decimal".to_string())?;
        if !metadata.is_dir()
            || metadata.uid() != roots.euid
            || metadata.dev() != device
            || metadata.ino() != inode
            || !named.is_dir()
            || named.file_type().is_symlink()
            || named.uid() != roots.euid
            || named.dev() != device
            || named.ino() != inode
        {
            return Err("repository identity changed while RepositoryGate was held".into());
        }
        Ok(common)
    }

    pub fn revalidate(
        &self,
        roots: &AuthorityRoots,
        repository: &RepositoryIdentityV1,
    ) -> Result<(), String> {
        self.revalidated_common_dir(roots, repository).map(|_| ())
    }

    pub fn admit_workspace_storage(
        &self,
        roots: &AuthorityRoots,
        repository: &RepositoryIdentityV1,
    ) -> Result<(), String> {
        self.revalidate(roots, repository)?;
        admit_ext4_path(&roots.authority)?;
        admit_ext4_path(&roots.data)?;
        admit_ext4_path(Path::new(&repository.common_dir_realpath))?;
        Ok(())
    }

    pub fn refuse_legacy_if_managed(
        &self,
        roots: &AuthorityRoots,
        repository: &RepositoryIdentityV1,
    ) -> Result<(), String> {
        let common = self.revalidated_common_dir(roots, repository)?;
        let authority = roots
            .authority
            .join("repositories")
            .join(&self.repository_id);
        let mut marker_metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
        let marker_exists = unsafe {
            libc::fstatat(
                common.as_raw_fd(),
                MARKER_RELATIVE_C.as_ptr().cast(),
                marker_metadata.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } == 0;
        if fs::symlink_metadata(&authority).is_ok() || marker_exists {
            return Err(
                "repository is in managed workspace mode; legacy fanout mutation is refused".into(),
            );
        }
        self.revalidate(roots, repository)
    }

    pub fn publish_workspace_marker(
        &self,
        roots: &AuthorityRoots,
        repository: &RepositoryIdentityV1,
        minimum_relay_version: &str,
        created_at: &str,
    ) -> Result<PathBuf, String> {
        self.revalidate(roots, repository)?;
        schema::Timestamp::parse(created_at)?;
        let authority_repository = roots
            .authority
            .join("repositories")
            .join(&repository.repository_id);
        if !authority_repository.is_dir() {
            return Err("workspace authority must be durable before marker publication".into());
        }
        let (directory, docks) = open_private_marker_directory(repository, roots.euid)?;
        let marker = WorkspaceAdmissionV1 {
            repository_id: repository.repository_id.clone(),
            mode: "workspace".into(),
            gate_protocol: GATE_PROTOCOL.into(),
            minimum_relay_version: minimum_relay_version.into(),
            authority_repository_path: authority_repository.to_string_lossy().into_owned(),
            created_at: created_at.into(),
        };
        let path = docks.join("workspace-admission-v1.json");
        if let Some(bytes) = read_marker_at(&directory, roots.euid)? {
            let parsed = WorkspaceAdmissionV1::from_jcs(schema::parse_jcs(&bytes, true)?)?;
            if !parsed.has_same_contract(&marker) {
                return Err(
                    "managed workspace marker exists with different identity or contract".into(),
                );
            }
            return Ok(path);
        }
        let bytes = schema::serialize_jcs_lf(&marker);
        let mut file = match open_marker_at(
            &directory,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
            0o600,
        ) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let existing = read_marker_at(&directory, roots.euid)?
                    .ok_or_else(|| "managed marker disappeared during publication".to_string())?;
                let parsed = WorkspaceAdmissionV1::from_jcs(schema::parse_jcs(&existing, true)?)?;
                if !parsed.has_same_contract(&marker) {
                    return Err(
                        "managed workspace marker exists with different identity or contract"
                            .into(),
                    );
                }
                return Ok(path);
            }
            Err(error) => return Err(format!("create managed marker: {error}")),
        };
        file.write_all(&bytes)
            .and_then(|_| file.sync_all())
            .map_err(|e| format!("persist managed marker: {e}"))?;
        directory
            .sync_all()
            .map_err(|e| format!("fsync managed marker directory: {e}"))?;
        Ok(path)
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceAdmissionV1 {
    pub repository_id: String,
    pub mode: String,
    pub gate_protocol: String,
    pub minimum_relay_version: String,
    pub authority_repository_path: String,
    pub created_at: String,
}
impl WorkspaceAdmissionV1 {
    fn has_same_contract(&self, other: &Self) -> bool {
        self.repository_id == other.repository_id
            && self.mode == other.mode
            && self.gate_protocol == other.gate_protocol
            && self.minimum_relay_version == other.minimum_relay_version
            && self.authority_repository_path == other.authority_repository_path
    }
}
impl ClosedJcs for WorkspaceAdmissionV1 {
    fn from_jcs(value: JcsValue) -> Result<Self, String> {
        let object = value.object()?;
        let keys = [
            "schema",
            "repository_id",
            "mode",
            "gate_protocol",
            "minimum_relay_version",
            "authority_repository_path",
            "created_at",
        ];
        if object.len() != keys.len() || keys.iter().any(|k| !object.contains_key(*k)) {
            return Err("WorkspaceAdmissionV1 keys differ".into());
        }
        let s = |k: &str| object[k].as_str().map(str::to_string);
        if s("schema")? != schema::SCHEMA_V1 {
            return Err("workspace marker schema mismatch".into());
        }
        schema::Sha256Digest::parse(&s("repository_id")?)?;
        if s("mode")? != "workspace" || s("gate_protocol")? != GATE_PROTOCOL {
            return Err("workspace marker mode/protocol mismatch".into());
        }
        schema::AbsPath::parse(&s("authority_repository_path")?)?;
        schema::Timestamp::parse(&s("created_at")?)?;
        Ok(Self {
            repository_id: s("repository_id")?,
            mode: s("mode")?,
            gate_protocol: s("gate_protocol")?,
            minimum_relay_version: s("minimum_relay_version")?,
            authority_repository_path: s("authority_repository_path")?,
            created_at: s("created_at")?,
        })
    }
    fn to_jcs(&self) -> JcsValue {
        JcsValue::Object(BTreeMap::from([
            (
                "authority_repository_path".into(),
                JcsValue::String(self.authority_repository_path.clone()),
            ),
            (
                "created_at".into(),
                JcsValue::String(self.created_at.clone()),
            ),
            (
                "gate_protocol".into(),
                JcsValue::String(self.gate_protocol.clone()),
            ),
            (
                "minimum_relay_version".into(),
                JcsValue::String(self.minimum_relay_version.clone()),
            ),
            ("mode".into(), JcsValue::String(self.mode.clone())),
            (
                "repository_id".into(),
                JcsValue::String(self.repository_id.clone()),
            ),
            ("schema".into(), JcsValue::String(schema::SCHEMA_V1.into())),
        ]))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MountAdmission {
    pub mount_id: u64,
    pub filesystem_type: String,
}
pub fn admit_ext4_path(path: &Path) -> Result<MountAdmission, String> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(path)
        .map_err(|e| format!("open authoritative directory {}: {e}", path.display()))?;
    admit_ext4_fd(&file)
}
pub fn admit_ext4_fd(file: &File) -> Result<MountAdmission, String> {
    #[cfg(target_os = "linux")]
    {
        let mut stat = std::mem::MaybeUninit::<libc::statx>::zeroed();
        let result = unsafe {
            libc::statx(
                file.as_raw_fd(),
                c"".as_ptr(),
                libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW,
                libc::STATX_MNT_ID,
                stat.as_mut_ptr(),
            )
        };
        if result != 0 {
            return Err(format!(
                "statx STATX_MNT_ID failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        let stat = unsafe { stat.assume_init() };
        if stat.stx_mask & libc::STATX_MNT_ID == 0 {
            return Err("statx did not report STATX_MNT_ID".into());
        }
        let mount_id = stat.stx_mnt_id;
        let mut text = String::new();
        File::open("/proc/self/mountinfo")
            .and_then(|mut f| f.read_to_string(&mut text))
            .map_err(|e| format!("read /proc/self/mountinfo: {e}"))?;
        let mut found = None;
        for line in text.lines() {
            let mut split = line.split(" - ");
            let left = split.next().unwrap_or("");
            let right = split.next();
            if split.next().is_some() || right.is_none() {
                continue;
            }
            let Some(id) = left
                .split_whitespace()
                .next()
                .and_then(|v| v.parse::<u64>().ok())
            else {
                continue;
            };
            if id == mount_id {
                let filesystem = right.unwrap().split_whitespace().next().unwrap_or("");
                found = Some(filesystem.to_string());
                break;
            }
        }
        let filesystem_type = found
            .ok_or_else(|| format!("mount ID {mount_id} is absent from /proc/self/mountinfo"))?;
        if filesystem_type != "ext4" {
            return Err(format!(
                "managed workspace requires exact ext4; mount ID {mount_id} is {filesystem_type}"
            ));
        }
        Ok(MountAdmission {
            mount_id,
            filesystem_type,
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = file;
        Err("managed workspace filesystem admission is unavailable on this platform".into())
    }
}

#[cfg(test)]
mod tests {
    use super::super::schema::ObjectFormat;
    use super::*;
    use std::os::unix::fs::symlink;

    fn fixture() -> (PathBuf, AuthorityRoots, RepositoryIdentityV1) {
        let base = PathBuf::from("/dev/shm")
            .join(format!("session-relay-gate-{}", crate::store::uuid_v4()));
        let roots = AuthorityRoots {
            authority: base.join("authority"),
            data: base.join("data"),
            euid: unsafe { libc::geteuid() },
        };
        authority::ensure_private_directory(&roots.authority, roots.euid).unwrap();
        authority::ensure_private_directory(&roots.data, roots.euid).unwrap();
        authority::ensure_private_directory(&roots.authority.join("repository-gates"), roots.euid)
            .unwrap();
        authority::ensure_private_directory(&roots.authority.join("repositories"), roots.euid)
            .unwrap();
        let repository = RepositoryIdentityV1 {
            repository_id: "a".repeat(64),
            common_dir_realpath: base.join("common").to_string_lossy().into_owned(),
            common_dir_dev: "1".into(),
            common_dir_ino: "1".into(),
            common_dir_owner_euid: roots.euid.to_string(),
            euid: roots.euid.to_string(),
            object_format: ObjectFormat::Sha1,
        };
        (base, roots, repository)
    }

    #[test]
    fn portable_gate_does_not_require_managed_ext4_admission() {
        let (base, roots, repository) = fixture();
        let gate = RepositoryGate::acquire(&roots, &repository).unwrap();
        assert_eq!(gate.repository_id(), repository.repository_id);
        assert!(gate.admit_workspace_storage(&roots, &repository).is_err());
        drop(gate);
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn legacy_gate_rejects_replaced_common_directory_identity() {
        let (base, roots, mut repository) = fixture();
        let common = PathBuf::from(&repository.common_dir_realpath);
        fs::create_dir(&common).unwrap();
        let metadata = fs::metadata(&common).unwrap();
        repository.common_dir_dev = metadata.dev().to_string();
        repository.common_dir_ino = metadata.ino().to_string();
        let gate = RepositoryGate::acquire(&roots, &repository).unwrap();
        fs::rename(&common, base.join("original-common")).unwrap();
        fs::create_dir(&common).unwrap();
        let error = gate
            .refuse_legacy_if_managed(&roots, &repository)
            .unwrap_err();
        assert!(error.contains("identity changed"), "{error}");
        drop(gate);
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn legacy_gate_rejects_symlink_swapped_common_directory_alias() {
        let (base, roots, mut repository) = fixture();
        let common = PathBuf::from(&repository.common_dir_realpath);
        fs::create_dir(&common).unwrap();
        let metadata = fs::metadata(&common).unwrap();
        repository.common_dir_dev = metadata.dev().to_string();
        repository.common_dir_ino = metadata.ino().to_string();
        let alias = base.join("common-alias");
        fs::create_dir(&alias).unwrap();
        let gate = RepositoryGate::acquire(&roots, &repository).unwrap();
        fs::remove_dir(&common).unwrap();
        symlink(&alias, &common).unwrap();
        let error = gate
            .refuse_legacy_if_managed(&roots, &repository)
            .unwrap_err();
        assert!(
            error.contains("reopen Git common directory") || error.contains("identity changed"),
            "{error}"
        );
        drop(gate);
        fs::remove_dir_all(base).unwrap();
    }
    #[test]
    fn marker_publication_refuses_symlinked_private_directory() {
        let (base, roots, mut repository) = fixture();
        fs::create_dir(Path::new(&repository.common_dir_realpath)).unwrap();
        let common_metadata = fs::metadata(&repository.common_dir_realpath).unwrap();
        repository.common_dir_dev = common_metadata.dev().to_string();
        repository.common_dir_ino = common_metadata.ino().to_string();
        let outside = base.join("outside");
        fs::create_dir(&outside).unwrap();
        symlink(
            &outside,
            Path::new(&repository.common_dir_realpath).join("docks"),
        )
        .unwrap();
        authority::ensure_private_directory(
            &roots
                .authority
                .join("repositories")
                .join(&repository.repository_id),
            roots.euid,
        )
        .unwrap();
        let gate = RepositoryGate::acquire(&roots, &repository).unwrap();
        assert!(
            gate.publish_workspace_marker(&roots, &repository, "1.0.0", "2026-07-22T00:00:00.000Z")
                .is_err()
        );
        assert!(!outside.join("workspace-admission-v1.json").exists());
        drop(gate);
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn marker_publication_is_private_durable_and_idempotent() {
        let (base, roots, mut repository) = fixture();
        fs::create_dir(Path::new(&repository.common_dir_realpath)).unwrap();
        let metadata = fs::metadata(&repository.common_dir_realpath).unwrap();
        repository.common_dir_dev = metadata.dev().to_string();
        repository.common_dir_ino = metadata.ino().to_string();
        authority::ensure_private_directory(
            &roots
                .authority
                .join("repositories")
                .join(&repository.repository_id),
            roots.euid,
        )
        .unwrap();
        let gate = RepositoryGate::acquire(&roots, &repository).unwrap();
        let first = gate
            .publish_workspace_marker(&roots, &repository, "1.0.0", "2026-07-22T00:00:00.000Z")
            .unwrap();
        let second = gate
            .publish_workspace_marker(&roots, &repository, "1.0.0", "2026-07-22T00:00:00.000Z")
            .unwrap();
        assert_eq!(first, second);
        let docks = fs::metadata(first.parent().unwrap()).unwrap();
        let marker = fs::metadata(&first).unwrap();
        assert_eq!(docks.mode() & 0o777, 0o700);
        assert_eq!(marker.mode() & 0o777, 0o600);
        assert_eq!(marker.nlink(), 1);
        drop(gate);
        fs::remove_dir_all(base).unwrap();
    }
}
