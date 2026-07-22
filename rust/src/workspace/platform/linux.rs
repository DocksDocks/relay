use crate::sha256::hex_digest;
use crate::workspace::custody::{
    worker_prepared_evidence, ControlEndpoint, LeaseReference, PacketKind,
    PayloadValue, ReceivedPacket,
};
use std::collections::BTreeMap;
use std::ffi::{CString, OsStr};
use std::fs;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const CGROUP2_SUPER_MAGIC: libc::c_long = 0x6367_7270;
const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1;
const LANDLOCK_RULE_PATH_BENEATH: i32 = 1;
const LANDLOCK_ACCESS_FS_EXECUTE: u64 = 1 << 0;
const LANDLOCK_ACCESS_FS_WRITE_FILE: u64 = 1 << 1;
const LANDLOCK_ACCESS_FS_READ_FILE: u64 = 1 << 2;
const LANDLOCK_ACCESS_FS_READ_DIR: u64 = 1 << 3;
const LANDLOCK_ACCESS_FS_REMOVE_DIR: u64 = 1 << 4;
const LANDLOCK_ACCESS_FS_REMOVE_FILE: u64 = 1 << 5;
const LANDLOCK_ACCESS_FS_MAKE_CHAR: u64 = 1 << 6;
const LANDLOCK_ACCESS_FS_MAKE_DIR: u64 = 1 << 7;
const LANDLOCK_ACCESS_FS_MAKE_REG: u64 = 1 << 8;
const LANDLOCK_ACCESS_FS_MAKE_SOCK: u64 = 1 << 9;
const LANDLOCK_ACCESS_FS_MAKE_FIFO: u64 = 1 << 10;
const LANDLOCK_ACCESS_FS_MAKE_BLOCK: u64 = 1 << 11;
const LANDLOCK_ACCESS_FS_MAKE_SYM: u64 = 1 << 12;
const LANDLOCK_ACCESS_FS_REFER: u64 = 1 << 13;
const LANDLOCK_ACCESS_FS_TRUNCATE: u64 = 1 << 14;
const LANDLOCK_READ_EXEC: u64 = LANDLOCK_ACCESS_FS_EXECUTE | LANDLOCK_ACCESS_FS_READ_FILE | LANDLOCK_ACCESS_FS_READ_DIR;
const LANDLOCK_WRITE: u64 = LANDLOCK_ACCESS_FS_WRITE_FILE
    | LANDLOCK_ACCESS_FS_REMOVE_DIR | LANDLOCK_ACCESS_FS_REMOVE_FILE
    | LANDLOCK_ACCESS_FS_MAKE_CHAR | LANDLOCK_ACCESS_FS_MAKE_DIR
    | LANDLOCK_ACCESS_FS_MAKE_REG | LANDLOCK_ACCESS_FS_MAKE_SOCK
    | LANDLOCK_ACCESS_FS_MAKE_FIFO | LANDLOCK_ACCESS_FS_MAKE_BLOCK
    | LANDLOCK_ACCESS_FS_MAKE_SYM | LANDLOCK_ACCESS_FS_REFER | LANDLOCK_ACCESS_FS_TRUNCATE;
const LANDLOCK_HANDLED: u64 = LANDLOCK_READ_EXEC | LANDLOCK_WRITE;
const CLONE_PIDFD: u64 = 0x0000_1000;
const CLONE_INTO_CGROUP: u64 = 0x2000_0000_0;
const P_PIDFD: libc::idtype_t = 3;
const EMPTY_DEADLINE: Duration = Duration::from_secs(10);
const PREPARED_DEADLINE: Duration = Duration::from_secs(10);

#[repr(C)]
struct LandlockRulesetAttr { handled_access_fs: u64 }
#[repr(C)]
struct LandlockPathBeneathAttr { allowed_access: u64, parent_fd: u64 }
#[repr(C)]
#[derive(Default)]
struct CloneArgs {
    flags: u64, pidfd: u64, child_tid: u64, parent_tid: u64, exit_signal: u64,
    stack: u64, stack_size: u64, tls: u64, set_tid: u64, set_tid_size: u64, cgroup: u64,
}

#[derive(Debug)]
pub struct DelegatedCgroup {
leaf: PathBuf,
membership: String,
directory: OwnedFd,
events: OwnedFd,
procs: OwnedFd,
kill: OwnedFd, }

#[derive(Clone, Debug)]
pub struct LandlockPolicy { pub workspace: PathBuf,
pub readable: Vec<PathBuf>,
pub writable_resources: Vec<PathBuf>, }

#[derive(Clone, Debug)]
pub struct WorkerLaunch { pub executable: PathBuf,
pub arguments: Vec<String>,
pub environment: BTreeMap<String, String>,
pub cwd: PathBuf,
pub resource_fds: Vec<RawFd>,
pub sandbox: LandlockPolicy, }

#[derive(Debug)]
pub struct ProcessIdentity { pub pid: libc::pid_t,
pub pidfd: OwnedFd,
pub start_token: String, }

#[derive(Debug)]
pub struct PreparedWorker { pub identity: ProcessIdentity,
pub prepared_evidence: PreparedEvidence,
activation: Option<OwnedFd>,
exec_status: OwnedFd,
expected_executable_dev: u64,
expected_executable_ino: u64,
membership: String,
failure_kill: OwnedFd,
failure_leaf: PathBuf, }

#[derive(Debug)]
pub struct VerifiedWorker {
    identity: ProcessIdentity,
    evidence: ActivatedEvidence,
    failure_kill: OwnedFd,
    failure_leaf: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedEvidence {
    pub pid: libc::pid_t,
    pub start_token: String,
    pub cgroup_membership: String,
    pub sandbox_prepared: bool,
    pub evidence_sha256: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActivatedEvidence { pub pid: libc::pid_t,
pub start_token: String,
pub executable_dev: u64,
pub executable_ino: u64,
pub evidence_sha256: String, }

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EmptyEvidence { pub cgroup_path: String,
pub populated: bool,
pub evidence_sha256: String, }

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LeaseProbeEvidence { pub dev: u64,
pub ino: u64,
pub evidence_sha256: String, }

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Ext4MountIdentity {
    pub mount_id: u64,
    pub mount_point: PathBuf,
    pub filesystem_type: String,
}

pub fn require_ext4_fd(fd: RawFd) -> Result<Ext4MountIdentity, String> {
    let mut statx: libc::statx = unsafe { std::mem::zeroed() };
    let empty = b"\0";
    let rc = unsafe {
        libc::statx(
            fd,
            empty.as_ptr().cast(),
            libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW,
            libc::STATX_MNT_ID,
            &mut statx,
        )
    };
    if rc != 0 || statx.stx_mask & libc::STATX_MNT_ID == 0 {
        return Err(format!(
            "authoritative FD has no STATX_MNT_ID: {}",
            std::io::Error::last_os_error()
        ));
    }
    let mount_id = statx.stx_mnt_id;
    let mountinfo = fs::read_to_string("/proc/self/mountinfo")
        .map_err(|error| format!("read mountinfo for authoritative FD: {error}"))?;
    let mut found = None;
    for line in mountinfo.lines() {
        let Some((before, after)) = line.split_once(" - ") else {
            return Err("malformed /proc/self/mountinfo row".to_string());
        };
        let fields = before.split_ascii_whitespace().collect::<Vec<_>>();
        if fields.len() < 6 {
            return Err("short /proc/self/mountinfo row".to_string());
        }
        if fields[0].parse::<u64>().ok() != Some(mount_id) {
            continue;
        }
        if found.is_some() {
            return Err("STATX_MNT_ID maps to multiple mountinfo rows".to_string());
        }
        let filesystem_type = after
            .split_ascii_whitespace()
            .next()
            .ok_or_else(|| "mountinfo row has no filesystem type".to_string())?;
        found = Some(Ext4MountIdentity {
            mount_id,
            mount_point: PathBuf::from(unescape_mountinfo(fields[4])?),
            filesystem_type: filesystem_type.to_string(),
        });
    }
    let identity =
        found.ok_or_else(|| "STATX_MNT_ID has no mountinfo row".to_string())?;
    if identity.filesystem_type != "ext4" {
        return Err(format!(
            "authoritative FD filesystem is {}; exact ext4 is required",
            identity.filesystem_type
        ));
    }
    Ok(identity)
}

pub fn admit() -> Result<(), String> { if landlock_abi()? < 3 { return Err("Linux custody requires Landlock ABI >= 3".to_string()); }
let probe = unsafe { libc::syscall(libc::SYS_pidfd_open, libc::getpid(), 0) } as RawFd;
if probe < 0 { return Err(format!("Linux custody requires pidfd_open: {}", std::io::Error::last_os_error())); }
unsafe { libc::close(probe); }
let root = delegated_root()?;
validate_delegation(&root) }

pub fn delegated_root() -> Result<PathBuf, String> {
    if let Some(root) = std::env::var_os("SESSION_RELAY_TEST_CGROUP_ROOT") {
        let root = PathBuf::from(root);
        if !root.is_absolute() {
            return Err(
                "SESSION_RELAY_TEST_CGROUP_ROOT must be absolute".to_string(),
            );
        }
        let metadata = fs::symlink_metadata(&root).map_err(|error| {
            format!("stat test cgroup delegation {}: {error}", root.display())
        })?;
        if metadata.file_type().is_symlink() {
            return Err(
                "SESSION_RELAY_TEST_CGROUP_ROOT may not be a symlink".to_string(),
            );
        }
        let canonical = fs::canonicalize(&root).map_err(|error| {
            format!("open test cgroup delegation {}: {error}", root.display())
        })?;
        if canonical != root {
            return Err(
                "SESSION_RELAY_TEST_CGROUP_ROOT must already be canonical"
                    .to_string(),
            );
        }
        return Ok(canonical);
    }
    Ok(PathBuf::from(format!(
        "/sys/fs/cgroup/session-relay-{}",
        unsafe { libc::geteuid() }
    )))
}

impl DelegatedCgroup {
    pub fn create(session_id: &str) -> Result<Self, String> { validate_leaf_name(session_id)?;
    let root = delegated_root()?;
    validate_delegation(&root)?;
    let leaf = root.join(session_id);
    match fs::create_dir(&leaf) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if read_populated(&leaf)? { return Err("existing session cgroup is populated".to_string()); }
            if has_child_cgroups(&leaf)? { return Err("existing session cgroup has child cgroups".to_string()); }
        }
        Err(error) => return Err(format!("create session cgroup {}: {error}", leaf.display())),
    }
    validate_cgroup_dir(&leaf, unsafe { libc::geteuid() })?;
    if fs::read_to_string(leaf.join("cgroup.type")).map_err(|e| format!("read cgroup.type: {e}"))?.trim() != "domain" {
        return Err("session cgroup is threaded or not a domain leaf".to_string());
    }
    if has_child_cgroups(&leaf)? { return Err("session cgroup must have no child cgroups".to_string()); }
    for required in ["cgroup.kill", "cgroup.events", "cgroup.procs"] {
        if !leaf.join(required).exists() { return Err(format!("session cgroup is missing mandatory {required}")); }
    }
    let directory = open_fd(&leaf, libc::O_RDONLY | libc::O_DIRECTORY)?;
    let events = open_fd(&leaf.join("cgroup.events"), libc::O_RDONLY)?;
    let procs = open_fd(&leaf.join("cgroup.procs"), libc::O_WRONLY)?;
    let kill = open_fd(&leaf.join("cgroup.kill"), libc::O_WRONLY)?;
    let membership = cgroup_membership_for_path(&leaf)?;
    Ok(Self { leaf, membership, directory, events, procs, kill }) }

    pub fn path(&self) -> &Path { &self.leaf }
    pub fn membership(&self) -> &str { &self.membership }

    pub fn duplicate_bootstrap_fds(&self, lease_fd: RawFd) -> Result<[OwnedFd; 4], String> { let fds = [lease_fd, self.directory.as_raw_fd(), self.events.as_raw_fd(), self.procs.as_raw_fd()];
    let duplicated = fds.map(dup_cloexec).into_iter().collect::<Result<Vec<_>, _>>()?;
    let array: [OwnedFd; 4] = duplicated.try_into().map_err(|_| "bootstrap FD duplication count changed".to_string())?;
    validate_bootstrap_fds(&array)?;
    Ok(array) }

    pub fn send_bootstrap(&self,
    endpoint: &mut ControlEndpoint,
    lease_fd: RawFd,) -> Result<u64, String> { let fds = self.duplicate_bootstrap_fds(lease_fd)?;
    let mut payload = BTreeMap::new();
    payload.insert("cgroup_membership".to_string(), PayloadValue::String(self.membership.clone()));
    let raw = fds.iter().map(AsRawFd::as_raw_fd).collect::<Vec<_>>();
    endpoint.send(PacketKind::Bootstrap, payload, &raw) }

    pub fn receive_bootstrap(received: ReceivedPacket) -> Result<[OwnedFd; 4], String> { if received.packet.kind != PacketKind::Bootstrap { return Err("supervisor expected BOOTSTRAP".to_string()); }
    let fds: [OwnedFd; 4] = received.fds.try_into().map_err(|_| "BOOTSTRAP requires exactly four FDs".to_string())?;
    validate_bootstrap_fds(&fds)?;
    Ok(fds) }


    pub fn from_bootstrap(
        fds: [OwnedFd; 4],
        expected_membership: &str,
    ) -> Result<(LeaseReference, Self), String> {
        validate_bootstrap_fds(&fds)?;
        let [lease, directory, events, procs] = fds;
        let leaf = fs::read_link(format!("/proc/self/fd/{}", directory.as_raw_fd()))
            .map_err(|error| format!("resolve bootstrap cgroup directory FD: {error}"))?;
        let leaf = fs::canonicalize(&leaf)
            .map_err(|error| format!("canonicalize bootstrap cgroup leaf: {error}"))?;
        let events_target =
            fs::read_link(format!("/proc/self/fd/{}", events.as_raw_fd()))
                .map_err(|error| {
                    format!("resolve bootstrap cgroup.events FD: {error}")
                })?;
        let procs_target =
            fs::read_link(format!("/proc/self/fd/{}", procs.as_raw_fd()))
                .map_err(|error| {
                    format!("resolve bootstrap cgroup.procs FD: {error}")
                })?;
        if events_target != leaf.join("cgroup.events")
            || procs_target != leaf.join("cgroup.procs")
        {
            return Err(
                "BOOTSTRAP cgroup control FDs do not belong to the leaf"
                    .to_string(),
            );
        }
        validate_cgroup_dir(&leaf, unsafe { libc::geteuid() })?;
        if has_child_cgroups(&leaf)? {
            return Err("bootstrap cgroup is not a leaf".to_string());
        }
        let membership = cgroup_membership_for_path(&leaf)?;
        if membership != expected_membership {
            return Err("bootstrap cgroup membership changed".to_string());
        }
        let kill = openat_fd(
            directory.as_raw_fd(),
            "cgroup.kill",
            libc::O_WRONLY,
        )?;
        let lease = LeaseReference::from_owned_fd(lease)?;
        Ok((
            lease,
            Self {
                leaf,
                membership,
                directory,
                events,
                procs,
                kill,
            },
        ))
    }
    pub fn launch_worker(&self, launch: &WorkerLaunch) -> Result<PreparedWorker, String> { validate_launch(launch)?;
    let executable = fs::metadata(&launch.executable).map_err(|e| format!("stat worker executable {}: {e}", launch.executable.display()))?;
    let (activation_read, activation_write) = pipe_cloexec()?;
    let (prepared_read, prepared_write) = pipe_cloexec()?;
    let (exec_read, exec_write) = pipe_cloexec()?;
    let argv = c_argv(&launch.executable, &launch.arguments)?;
    let failure_kill = dup_cloexec(self.kill.as_raw_fd())?;
    let env = c_env(&launch.environment)?;
    let cwd = c_path(&launch.cwd)?;
    let child = unsafe {
        clone_or_fork_into_cgroup(self.directory.as_raw_fd())?
    };
    if child.pid == 0 {
        drop(activation_write); drop(prepared_read); drop(exec_read);
        let result = child_prepare_and_exec(
            &argv, &env, &cwd, &launch.sandbox, &launch.resource_fds,
            activation_read.as_raw_fd(), prepared_write.as_raw_fd(), exec_write.as_raw_fd(),
        );
        let errno = result.err().and_then(|e| e.raw_os_error()).unwrap_or(libc::EPERM);
        let bytes = errno.to_ne_bytes();
        unsafe { libc::write(exec_write.as_raw_fd(), bytes.as_ptr().cast(), bytes.len()); libc::_exit(127); }
    }
    drop(activation_read); drop(prepared_write); drop(exec_write);
    let pid = child.pid;
    let pidfd = if child.pidfd >= 0 {
        if let Err(error) = set_cloexec(child.pidfd) {
            return Err(cleanup_failed_launch(
                self.kill.as_raw_fd(),
                &self.leaf,
                pid,
                None,
                error,
            ));
        }
        unsafe { OwnedFd::from_raw_fd(child.pidfd) }
    } else {
        match pidfd_open(pid) {
            Ok(pidfd) => pidfd,
            Err(error) => {
                return Err(cleanup_failed_launch(
                    self.kill.as_raw_fd(),
                    &self.leaf,
                    pid,
                    None,
                    error,
                ));
            }
        }
    };
    if child.used_fallback {
        if let Err(error) =
            write_all_fd(self.procs.as_raw_fd(), format!("{pid}").as_bytes())
        {
            return Err(cleanup_failed_launch(
                self.kill.as_raw_fd(),
                &self.leaf,
                pid,
                Some(&pidfd),
                error,
            ));
        }
    }
    let start_token = process_start_token(pid).map_err(|error| {
        cleanup_failed_launch(
            self.kill.as_raw_fd(),
            &self.leaf,
            pid,
            Some(&pidfd),
            error,
        )
    })?;
    prove_exact_membership(pid, &self.membership).map_err(|error| {
        cleanup_failed_launch(
            self.kill.as_raw_fd(),
            &self.leaf,
            pid,
            Some(&pidfd),
            error,
        )
    })?;
    let prepared =
        read_one_with_deadline(prepared_read.as_raw_fd(), PREPARED_DEADLINE)
            .map_err(|error| {
                cleanup_failed_launch(
                    self.kill.as_raw_fd(),
                    &self.leaf,
                    pid,
                    Some(&pidfd),
                    error,
                )
            })?;
    if prepared != 0x01 {
        return Err(cleanup_failed_launch(
            self.kill.as_raw_fd(),
            &self.leaf,
            pid,
            Some(&pidfd),
            "worker sandbox-prepared byte is not 0x01".to_string(),
        ));
    }
    let prepared_evidence = PreparedEvidence {
        pid,
        start_token: start_token.clone(),
        cgroup_membership: self.membership.clone(),
        sandbox_prepared: true,
        evidence_sha256: worker_prepared_evidence(
            pid,
            &start_token,
            &self.membership,
        )?,
    };
    Ok(PreparedWorker {
        identity: ProcessIdentity { pid, pidfd, start_token },
        prepared_evidence,
        activation: Some(activation_write),
        exec_status: exec_read,
        expected_executable_dev: executable.dev(),
        expected_executable_ino: executable.ino(),
        membership: self.membership.clone(),
        failure_kill,
        failure_leaf: self.leaf.clone(),
    }) }

    pub fn verify_pre_activation(
        &self,
        root: &ProcessIdentity,
    ) -> Result<(), String> {
        if !pidfd_is_live(root)? {
            return Err("worker root pidfd is not live before activation".to_string());
        }
        if process_start_token(root.pid)? != root.start_token {
            return Err(
                "worker root start token changed before activation".to_string(),
            );
        }
        prove_exact_membership(root.pid, &self.membership)?;
        if !read_populated(&self.leaf)? {
            return Err(
                "cgroup.events is not populated before activation".to_string(),
            );
        }
        Ok(())
    }

    pub fn kill_and_wait_empty(&self, root: &ProcessIdentity) -> Result<EmptyEvidence, String> { write_all_fd(self.kill.as_raw_fd(), b"1")?;
    wait_recursive_empty(&self.leaf, EMPTY_DEADLINE)?;
    reap_pidfd(root)?;
    let evidence_sha256 = hex_digest(format!("cgroup-empty-v1\0{}\0populated=0", self.membership).as_bytes());
    Ok(EmptyEvidence { cgroup_path: self.membership.clone(), populated: false, evidence_sha256 }) }

    pub fn wait_empty(&self) -> Result<EmptyEvidence, String> { wait_recursive_empty(&self.leaf, EMPTY_DEADLINE)?;
    let evidence_sha256 = hex_digest(format!("cgroup-empty-v1\0{}\0populated=0", self.membership).as_bytes());
    Ok(EmptyEvidence { cgroup_path: self.membership.clone(), populated: false, evidence_sha256 }) }

    pub fn wait_root_and_empty(
        &self,
        root: &ProcessIdentity,
    ) -> Result<EmptyEvidence, String> {

        reap_pidfd(root)?;
        self.wait_empty()
    }

    pub fn remove(self) -> Result<(), String> { if read_populated(&self.leaf)? || has_child_cgroups(&self.leaf)? { return Err("refuse to remove nonempty session cgroup".to_string()); }
    let leaf = self.leaf.clone();
    drop(self);
    fs::remove_dir(&leaf).map_err(|e| format!("remove session cgroup {}: {e}", leaf.display())) }
}

impl PreparedWorker {
    pub fn abort(self, error: impl Into<String>) -> String {
        self.activation_failure(error.into())
    }

    pub fn verify_activation(
        mut self,
    ) -> Result<VerifiedWorker, String> {
        prove_exact_membership(self.identity.pid, &self.membership)
            .map_err(|error| self.activation_failure(error))?;
        write_all_fd(
            self.activation
                .as_ref()
                .expect("activation FD exists before barrier")
                .as_raw_fd(),
            &[0x01],
        )
        .map_err(|error| self.activation_failure(error))?;
        drop(self.activation.take());
        let mut status = [0_u8; 4];
        let count = read_with_deadline(
            self.exec_status.as_raw_fd(),
            &mut status,
            PREPARED_DEADLINE,
        )
        .map_err(|error| self.activation_failure(error))?;
        if count != 0 {
            let errno = if count == 4 {
                i32::from_ne_bytes(status)
            } else {
                libc::EIO
            };
            return Err(self.activation_failure(format!(
                "worker exec failed: {}",
                std::io::Error::from_raw_os_error(errno)
            )));
        }
        wait_for_traced_exec_stop(self.identity.pid)
            .map_err(|error| self.activation_failure(error))?;
        let current_start = process_start_token(self.identity.pid)
            .map_err(|error| self.activation_failure(error))?;
        if current_start != self.identity.start_token {
            return Err(self.activation_failure(
                "worker start token changed before activation".to_string(),
            ));
        }
        let executable =
            fs::metadata(format!("/proc/{}/exe", self.identity.pid))
                .map_err(|error| {
                    self.activation_failure(format!(
                        "stat activated worker executable: {error}"
                    ))
                })?;
        if executable.dev() != self.expected_executable_dev
            || executable.ino() != self.expected_executable_ino
        {
            return Err(self.activation_failure(
                "activated worker executable identity differs from admitted executable"
                    .to_string(),
            ));
        }
        let evidence_sha256 = hex_digest(
            format!(
                "worker-activated-v1\0{}\0{}\0{}\0{}\0{}",
                self.identity.pid,
                self.identity.start_token,
                self.membership,
                executable.dev(),
                executable.ino()
            )
            .as_bytes(),
        );
        let evidence = ActivatedEvidence {
            pid: self.identity.pid,
            start_token: self.identity.start_token.clone(),
            executable_dev: executable.dev(),
            executable_ino: executable.ino(),
            evidence_sha256,
        };
        Ok(VerifiedWorker {
            identity: self.identity,
            evidence,
            failure_kill: self.failure_kill,
            failure_leaf: self.failure_leaf,
        })
    }
    fn activation_failure(&self, error: String) -> String {
        cleanup_failed_launch(
            self.failure_kill.as_raw_fd(),
            &self.failure_leaf,
            self.identity.pid,
            Some(&self.identity.pidfd),
            error,
        )
    }
}

impl VerifiedWorker {
    pub fn release_after_ack<F>(
        self,
        acknowledge: F,
    ) -> Result<(ProcessIdentity, ActivatedEvidence), String>
    where
        F: FnOnce(&ActivatedEvidence) -> Result<(), String>,
    {
        if let Err(error) = acknowledge(&self.evidence) {
            return Err(self.activation_failure(error));
        }
        let rc = unsafe {
            libc::ptrace(
                libc::PTRACE_DETACH,
                self.identity.pid,
                std::ptr::null_mut::<libc::c_void>(),
                std::ptr::null_mut::<libc::c_void>(),
            )
        };
        if rc != 0 {
            return Err(self.activation_failure(format!(
                "release traced activation barrier: {}",
                std::io::Error::last_os_error()
            )));
        }
        Ok((self.identity, self.evidence))
    }

    pub fn abort(self, error: impl Into<String>) -> String {
        self.activation_failure(error.into())
    }

    fn activation_failure(&self, error: String) -> String {
        let _ = unsafe {
            libc::ptrace(
                libc::PTRACE_DETACH,
                self.identity.pid,
                std::ptr::null_mut::<libc::c_void>(),
                libc::SIGKILL as usize as *mut libc::c_void,
            )
        };
        cleanup_failed_launch(
            self.failure_kill.as_raw_fd(),
            &self.failure_leaf,
            self.identity.pid,
            Some(&self.identity.pidfd),
            error,
        )
    }
}


struct ProcessStat {
    state: char,
    start_token: String,
}

fn process_stat(pid: libc::pid_t) -> Result<ProcessStat, String> {
    if pid <= 0 {
        return Err("process PID must be positive".to_string());
    }
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))
        .map_err(|error| format!("read /proc/{pid}/stat: {error}"))?;
    let end = stat
        .rfind(") ")
        .ok_or_else(|| format!("malformed /proc/{pid}/stat"))?;
    let fields = stat[end + 2..].split_ascii_whitespace().collect::<Vec<_>>();
    let state = fields
        .first()
        .and_then(|value| value.chars().next())
        .ok_or_else(|| format!("/proc/{pid}/stat has no process state"))?;
    let start_token = fields
        .get(19)
        .ok_or_else(|| format!("/proc/{pid}/stat has no start token"))?;
    if start_token.is_empty()
        || !start_token.bytes().all(|byte| byte.is_ascii_digit())
        || start_token.starts_with('0')
    {
        return Err(format!(
            "/proc/{pid}/stat start token is not canonical"
        ));
    }
    Ok(ProcessStat {
        state,
        start_token: (*start_token).to_string(),
    })
}

pub fn process_start_token(pid: libc::pid_t) -> Result<String, String> {
    Ok(process_stat(pid)?.start_token)
}

pub fn validate_pidfd_identity(
    pidfd: RawFd,
    expected_pid: libc::pid_t,
    expected_start_token: &str,
) -> Result<(), String> {
    let flags = unsafe { libc::fcntl(pidfd, libc::F_GETFD) };
    if flags < 0 || flags & libc::FD_CLOEXEC == 0 {
        return Err("received pidfd is not CLOEXEC".to_string());
    }
    let fdinfo = fs::read_to_string(format!("/proc/self/fdinfo/{pidfd}"))
        .map_err(|error| format!("read received pidfd identity: {error}"))?;
    let mut reported_pid = None;
    for line in fdinfo.lines() {
        if let Some(value) = line.strip_prefix("Pid:\t") {
            if reported_pid.is_some() {
                return Err(
                    "received pidfd has duplicate Pid identity".to_string(),
                );
            }
            reported_pid = value.parse::<libc::pid_t>().ok();
        }
    }
    if reported_pid != Some(expected_pid) {
        return Err("received pidfd identifies a different process".to_string());
    }
    if process_start_token(expected_pid)? != expected_start_token {
        return Err("received pidfd process start token changed".to_string());
    }
    if !pidfd_raw_is_live(pidfd, expected_pid, expected_start_token)? {
        return Err("received pidfd process is not live".to_string());
    }
    Ok(())
}

pub fn pidfd_open(pid: libc::pid_t) -> Result<OwnedFd, String> { let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as RawFd;
if fd < 0 { return Err(format!("pidfd_open({pid}): {}", std::io::Error::last_os_error())); }
set_cloexec(fd)?;
Ok(unsafe { OwnedFd::from_raw_fd(fd) }) }

fn pidfd_raw_is_live(
    pidfd: RawFd,
    pid: libc::pid_t,
    start_token: &str,
) -> Result<bool, String> {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::waitid(
            P_PIDFD,
            pidfd as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if rc == 0 {
        return Ok(unsafe { info.si_pid() } == 0);
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ECHILD) {
        let stat = match process_stat(pid) {
            Ok(stat) => stat,
            Err(_) => return Ok(false),
        };
        if stat.start_token != start_token
            || matches!(stat.state, 'Z' | 'X' | 'x')
        {
            return Ok(false);
        }
        let live = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                pidfd,
                0,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        return Ok(live == 0);
    }
    Err(format!("waitid(P_PIDFD): {error}"))
}

pub fn pidfd_is_live(identity: &ProcessIdentity) -> Result<bool, String> {
    pidfd_raw_is_live(
        identity.pidfd.as_raw_fd(),
        identity.pid,
        &identity.start_token,
    )
}

pub fn signal_pidfd(identity: &ProcessIdentity, signal: i32) -> Result<(), String> { let rc = unsafe { libc::syscall(libc::SYS_pidfd_send_signal, identity.pidfd.as_raw_fd(), signal, std::ptr::null::<libc::siginfo_t>(), 0) };
if rc != 0 { return Err(format!("pidfd_send_signal: {}", std::io::Error::last_os_error())); }
Ok(()) }

pub fn probe_closed_lease(probe_fd: RawFd) -> Result<LeaseProbeEvidence, String> { let mut lock = libc::flock { l_type: libc::F_WRLCK as i16, l_whence: libc::SEEK_SET as i16, l_start: 0, l_len: 0, l_pid: 0 };
let rc = unsafe { libc::fcntl(probe_fd, libc::F_OFD_SETLK, &mut lock) };
if rc != 0 { return Err(format!("independent workspace lease probe did not acquire OFD lock: {}", std::io::Error::last_os_error())); }
let metadata = fstat_metadata(probe_fd)?;
let evidence_sha256 = hex_digest(format!("lease-closed-probe-v1\0{}\0{}", metadata.0, metadata.1).as_bytes());
Ok(LeaseProbeEvidence { dev: metadata.0, ino: metadata.1, evidence_sha256 }) }

pub fn validate_bootstrap_fds(fds: &[OwnedFd; 4]) -> Result<(), String> {
    for fd in fds {
        let descriptor_flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
        if descriptor_flags < 0 || descriptor_flags & libc::FD_CLOEXEC == 0 {
            return Err("BOOTSTRAP FD is not CLOEXEC".to_string());
        }
    }
    let status_flags = fds
        .iter()
        .map(|fd| unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) })
        .collect::<Vec<_>>();
    if status_flags.iter().any(|flags| *flags < 0)
        || status_flags[0] & libc::O_ACCMODE != libc::O_RDWR
        || status_flags[1] & libc::O_ACCMODE != libc::O_RDONLY
        || status_flags[2] & libc::O_ACCMODE != libc::O_RDONLY
        || status_flags[3] & libc::O_ACCMODE != libc::O_WRONLY
    {
        return Err("BOOTSTRAP cgroup FD access modes are not exact".to_string());
    }
    let modes = fds
        .iter()
        .map(|fd| fstat_mode(fd.as_raw_fd()))
        .collect::<Result<Vec<_>, _>>()?;
if modes[0] & libc::S_IFMT != libc::S_IFREG || modes[1] & libc::S_IFMT != libc::S_IFDIR
    || modes[2] & libc::S_IFMT != libc::S_IFREG || modes[3] & libc::S_IFMT != libc::S_IFREG
{ return Err("BOOTSTRAP FDs are not lease-file,cgroup-dir,events-file,procs-file".to_string()); }
for fd in &fds[1..] {
    let mut statfs: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatfs(fd.as_raw_fd(), &mut statfs) } != 0 || statfs.f_type != CGROUP2_SUPER_MAGIC {
        return Err("BOOTSTRAP cgroup FDs are not on cgroup v2".to_string());
    }
}
validate_incoming_lease_lock(fds[0].as_raw_fd())?;

fn validate_incoming_lease_lock(lease_fd: RawFd) -> Result<(), String> {
    let path = CString::new(format!("/proc/self/fd/{lease_fd}"))
        .map_err(|_| "lease descriptor path contains NUL".to_string())?;
    let probe = unsafe {
        libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC)
    };
    if probe < 0 {
        return Err(format!(
            "open independent BOOTSTRAP lease probe: {}",
            std::io::Error::last_os_error()
        ));
    }
    let probe = unsafe { OwnedFd::from_raw_fd(probe) };
    let mut lock = libc::flock {
        l_type: libc::F_WRLCK as i16,
        l_whence: libc::SEEK_SET as i16,
        l_start: 0,
        l_len: 0,
        l_pid: 0,
    };
    if unsafe { libc::fcntl(probe.as_raw_fd(), libc::F_OFD_SETLK, &mut lock) }
        == 0
    {
        lock.l_type = libc::F_UNLCK as i16;
        let _ = unsafe {
            libc::fcntl(probe.as_raw_fd(), libc::F_OFD_SETLK, &mut lock)
        };
        return Err(
            "BOOTSTRAP lease FD does not hold the lifetime OFD lock"
                .to_string(),
        );
    }
    let error = std::io::Error::last_os_error();
    if !matches!(
        error.raw_os_error(),
        Some(libc::EAGAIN) | Some(libc::EACCES)
    ) {
        return Err(format!("probe BOOTSTRAP lease OFD lock: {error}"));
    }
    Ok(())
}
Ok(()) }

fn validate_delegation(root: &Path) -> Result<(), String> {
    validate_cgroup_dir(root, unsafe { libc::geteuid() })?;
    let mut statfs: libc::statfs = unsafe { std::mem::zeroed() };
    let croot = c_path(root)?;
    if unsafe { libc::statfs(croot.as_ptr(), &mut statfs) } != 0 || statfs.f_type != CGROUP2_SUPER_MAGIC {
        return Err("custody delegation is not unified cgroup v2".to_string());
    }
    for required in ["cgroup.controllers", "cgroup.subtree_control"] {
        if !root.join(required).is_file() { return Err(format!("custody delegation is missing {required}")); }
    }
    Ok(())
}

fn validate_cgroup_dir(path: &Path, euid: libc::uid_t) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path).map_err(|e| format!("stat cgroup {}: {e}", path.display()))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() || metadata.uid() != euid as u32 {
        return Err(format!("cgroup {} is not an EUID-owned real directory", path.display()));
    }
    Ok(())
}

fn validate_leaf_name(value: &str) -> Result<(), String> {
    let bytes = value.as_bytes();
    if bytes.len() != 36
        || bytes[8] != b'-'
        || bytes[13] != b'-'
        || bytes[14] != b'4'
        || bytes[18] != b'-'
        || bytes[23] != b'-'
        || !matches!(bytes[19], b'8' | b'9' | b'a' | b'b')
        || bytes.iter().enumerate().any(|(index, byte)| {
            !matches!(index, 8 | 13 | 18 | 23)
                && !matches!(byte, b'0'..=b'9' | b'a'..=b'f')
        })
    {
        return Err("session cgroup leaf must be a lowercase UUIDv4".to_string());
    }
    Ok(())
}

fn validate_launch(launch: &WorkerLaunch) -> Result<(), String> {
    if !launch.executable.is_absolute() || !launch.cwd.is_absolute() || !launch.sandbox.workspace.is_absolute() {
        return Err("worker executable, cwd, and workspace must be absolute".to_string());
    }
    if launch.resource_fds.iter().any(|fd| *fd < 3) {
        return Err("resource FDs may not alias stdio".to_string());
    }
    let mut sorted = launch.resource_fds.clone();
    sorted.sort_unstable();
    sorted.dedup();
    if sorted.len() != launch.resource_fds.len() {
        return Err("worker resource FD inventory contains duplicates".to_string());
    }
    let mut projected = Vec::new();
    for (name, value) in &launch.environment {
        if name.is_empty()
            || name.contains('=')
            || name.as_bytes().contains(&0)
            || value.as_bytes().contains(&0)
        {
            return Err(
                "worker environment contains an invalid name or NUL".to_string(),
            );
        }
        if let Some(resource) = name
            .strip_prefix("DOCKS_RESOURCE_")
            .and_then(|name| name.strip_suffix("_FD"))
        {
            if resource.is_empty()
                || !resource
                    .bytes()
                    .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
            {
                return Err("resource FD environment name is invalid".to_string());
            }
            let fd = value
                .parse::<RawFd>()
                .map_err(|_| "resource FD environment value is not decimal".to_string())?;
            if fd.to_string() != *value {
                return Err(
                    "resource FD environment value is not canonical decimal".to_string(),
                );
            }
            projected.push(fd);
        }
    }
    projected.sort_unstable();
    if projected != sorted {
        return Err(
            "resource FD inventory is not projected one-to-one in the environment"
                .to_string(),
        );
    }
    Ok(())
}

struct ForkResult { pid: libc::pid_t, pidfd: RawFd, used_fallback: bool }
unsafe fn clone_or_fork_into_cgroup(cgroup_fd: RawFd) -> Result<ForkResult, String> {
    let mut pidfd: RawFd = -1;
    let args = CloneArgs { flags: CLONE_PIDFD | CLONE_INTO_CGROUP, pidfd: (&mut pidfd as *mut RawFd) as u64, exit_signal: libc::SIGCHLD as u64, cgroup: cgroup_fd as u64, ..CloneArgs::default() };
    let pid = unsafe { libc::syscall(libc::SYS_clone3, &args, std::mem::size_of::<CloneArgs>()) } as libc::pid_t;
    if pid >= 0 { return Ok(ForkResult { pid, pidfd, used_fallback: false }); }
    let error = std::io::Error::last_os_error();
    if !matches!(
        error.raw_os_error(),
        Some(libc::ENOSYS)
            | Some(libc::EINVAL)
            | Some(libc::E2BIG)
            | Some(libc::EOPNOTSUPP)
    ) {
        return Err(format!("clone3(CLONE_INTO_CGROUP): {error}"));
    }
    let pid = unsafe { libc::fork() };
    if pid < 0 { return Err(format!("fork blocked worker: {}", std::io::Error::last_os_error())); }
    Ok(ForkResult { pid, pidfd: -1, used_fallback: true })
}

fn wait_for_traced_exec_stop(pid: libc::pid_t) -> Result<(), String> {
    let mut status = 0;
    loop {
        let rc = unsafe { libc::waitpid(pid, &mut status, libc::WUNTRACED) };
        if rc == pid {
            break;
        }
        if rc < 0
            && std::io::Error::last_os_error().kind()
                == std::io::ErrorKind::Interrupted
        {
            continue;
        }
        return Err(format!(
            "wait for traced worker exec: {}",
            std::io::Error::last_os_error()
        ));
    }
    if !libc::WIFSTOPPED(status) || libc::WSTOPSIG(status) != libc::SIGTRAP {
        return Err("worker did not stop at traced exec barrier".to_string());
    }
    if unsafe {
        libc::ptrace(
            libc::PTRACE_SETOPTIONS,
            pid,
            std::ptr::null_mut::<libc::c_void>(),
            libc::PTRACE_O_EXITKILL as usize as *mut libc::c_void,
        )
    } != 0
    {
        return Err(format!(
            "arm traced worker exit-kill: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

fn child_prepare_and_exec(
    argv: &[CString], env: &[CString], cwd: &CString, policy: &LandlockPolicy, resource_fds: &[RawFd],
    activation_fd: RawFd, prepared_fd: RawFd, exec_fd: RawFd,
) -> Result<(), std::io::Error> {
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 { return Err(std::io::Error::last_os_error()); }
    apply_landlock(policy).map_err(std::io::Error::other)?;
    for fd in resource_fds {
        let flags = unsafe { libc::fcntl(*fd, libc::F_GETFD) };
        if flags < 0
            || unsafe { libc::fcntl(*fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) }
                != 0
        {
            return Err(std::io::Error::last_os_error());
        }
    }
    let mut keep = vec![0, 1, 2, activation_fd, prepared_fd, exec_fd]; keep.extend_from_slice(resource_fds); keep.sort_unstable(); keep.dedup();
    close_unlisted_fds(&keep)?;
    if unsafe { libc::chdir(cwd.as_ptr()) } != 0 { return Err(std::io::Error::last_os_error()); }
    if unsafe { libc::write(prepared_fd, [0x01_u8].as_ptr().cast(), 1) } != 1 { return Err(std::io::Error::last_os_error()); }
    unsafe { libc::close(prepared_fd); }
    let mut activation = 0_u8;
    if unsafe { libc::read(activation_fd, (&mut activation as *mut u8).cast(), 1) } != 1 || activation != 0x01 { return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "activation barrier closed or ambiguous")); }
    unsafe { libc::close(activation_fd); }
    if unsafe {
        libc::ptrace(
            libc::PTRACE_TRACEME,
            0,
            std::ptr::null_mut::<libc::c_void>(),
            std::ptr::null_mut::<libc::c_void>(),
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    let mut argv_ptrs = argv.iter().map(|v| v.as_ptr()).collect::<Vec<_>>();
    argv_ptrs.push(std::ptr::null());
    let mut env_ptrs = env.iter().map(|v| v.as_ptr()).collect::<Vec<_>>();
    env_ptrs.push(std::ptr::null());
    unsafe {
        libc::execve(argv[0].as_ptr(), argv_ptrs.as_ptr(), env_ptrs.as_ptr());
    }
    Err(std::io::Error::last_os_error())
}

fn landlock_abi() -> Result<i32, String> {
    let abi = unsafe { libc::syscall(libc::SYS_landlock_create_ruleset, std::ptr::null::<LandlockRulesetAttr>(), 0, LANDLOCK_CREATE_RULESET_VERSION) } as i32;
    if abi < 0 { return Err(format!("query Landlock ABI: {}", std::io::Error::last_os_error())); }
    Ok(abi)
}

fn apply_landlock(policy: &LandlockPolicy) -> Result<(), String> {
    if landlock_abi()? < 3 { return Err("Linux custody requires Landlock ABI >= 3".to_string()); }
    let attr = LandlockRulesetAttr { handled_access_fs: LANDLOCK_HANDLED };
    let fd = unsafe { libc::syscall(libc::SYS_landlock_create_ruleset, &attr, std::mem::size_of::<LandlockRulesetAttr>(), 0) } as RawFd;
    if fd < 0 { return Err(format!("create Landlock ruleset: {}", std::io::Error::last_os_error())); }
    let ruleset = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut readable = vec![PathBuf::from("/")]; readable.extend(policy.readable.iter().cloned());
    for path in readable { add_landlock_path(&ruleset, &path, LANDLOCK_READ_EXEC)?; }
    add_landlock_path(&ruleset, &policy.workspace, LANDLOCK_HANDLED)?;
    for path in &policy.writable_resources { add_landlock_path(&ruleset, path, LANDLOCK_HANDLED)?; }
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 { return Err(format!("set no-new-privs before Landlock: {}", std::io::Error::last_os_error())); }
    if unsafe { libc::syscall(libc::SYS_landlock_restrict_self, ruleset.as_raw_fd(), 0) } != 0 { return Err(format!("enforce Landlock ruleset: {}", std::io::Error::last_os_error())); }
    Ok(())
}

fn add_landlock_path(ruleset: &OwnedFd, path: &Path, access: u64) -> Result<(), String> {
    let parent = open_fd(path, libc::O_PATH)?;
    let attr = LandlockPathBeneathAttr { allowed_access: access, parent_fd: parent.as_raw_fd() as u64 };
    if unsafe { libc::syscall(libc::SYS_landlock_add_rule, ruleset.as_raw_fd(), LANDLOCK_RULE_PATH_BENEATH, &attr, 0) } != 0 {
        return Err(format!("add Landlock rule for {}: {}", path.display(), std::io::Error::last_os_error()));
    }
    Ok(())
}

fn prove_exact_membership(pid: libc::pid_t, expected: &str) -> Result<(), String> {
    let value = fs::read_to_string(format!("/proc/{pid}/cgroup")).map_err(|e| format!("read worker cgroup membership: {e}"))?;
    let rows = value.lines().collect::<Vec<_>>();
    if rows.len() != 1 || rows[0].strip_prefix("0::") != Some(expected) { return Err(format!("worker cgroup membership is not exact: expected {expected}")); }
    Ok(())
}

fn cgroup_membership_for_path(path: &Path) -> Result<String, String> {
    let mount = cgroup2_mountpoint()?;
    let relative = path.strip_prefix(&mount).map_err(|_| format!("cgroup {} is outside cgroup2 mount {}", path.display(), mount.display()))?;
    Ok(format!("/{}", relative.as_os_str().as_bytes().split(|b| *b == b'/').map(|part| String::from_utf8_lossy(part)).collect::<Vec<_>>().join("/")))
}

fn cgroup2_mountpoint() -> Result<PathBuf, String> {
    let mountinfo = fs::read_to_string("/proc/self/mountinfo").map_err(|e| format!("read mountinfo for cgroup v2: {e}"))?;
    let mut matches = Vec::new();
    for line in mountinfo.lines() {
        let Some((before, after)) = line.split_once(" - ") else { continue };
        if after.split_ascii_whitespace().next() != Some("cgroup2") { continue; }
        let fields = before.split_ascii_whitespace().collect::<Vec<_>>();
        if fields.len() >= 5 { matches.push(PathBuf::from(unescape_mountinfo(fields[4])?)); }
    }
    if matches.len() != 1 { return Err("expected exactly one cgroup2 mount".to_string()); }
    Ok(matches.remove(0))
}

fn unescape_mountinfo(value: &str) -> Result<String, String> {
    let mut out = Vec::new(); let bytes = value.as_bytes(); let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\' {
            if index + 3 >= bytes.len() { return Err("malformed mountinfo escape".to_string()); }
            let oct = &value[index + 1..index + 4];
            let byte = u8::from_str_radix(oct, 8).map_err(|_| "malformed mountinfo escape".to_string())?;
            out.push(byte); index += 4;
        } else { out.push(bytes[index]); index += 1; }
    }
    String::from_utf8(out).map_err(|_| "cgroup2 mountpoint is not UTF-8".to_string())
}

fn wait_recursive_empty(path: &Path, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        if has_child_cgroups(path)? { return Err("session cgroup gained a child cgroup; recursive empty proof is ambiguous".to_string()); }
        if !read_populated(path)? { return Ok(()); }
        if Instant::now() >= deadline { return Err("cgroup.events did not reach populated 0 before deadline".to_string()); }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn read_populated(path: &Path) -> Result<bool, String> {
    let events = fs::read_to_string(path.join("cgroup.events")).map_err(|e| format!("read cgroup.events: {e}"))?;
    let values = events.lines().filter_map(|line| line.split_once(' ')).collect::<BTreeMap<_, _>>();
    match values.get("populated") { Some(&"0") => Ok(false), Some(&"1") => Ok(true), _ => Err("cgroup.events has no exact populated value".to_string()) }
}

fn has_child_cgroups(path: &Path) -> Result<bool, String> {
    for entry in fs::read_dir(path).map_err(|e| format!("enumerate cgroup leaf: {e}"))? {
        let entry = entry.map_err(|e| format!("enumerate cgroup leaf entry: {e}"))?;
        if entry.file_type().map_err(|e| format!("stat cgroup leaf entry: {e}"))?.is_dir() { return Ok(true); }
    }
    Ok(false)
}

fn reap_pidfd(identity: &ProcessIdentity) -> Result<(), String> {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::waitid(P_PIDFD, identity.pidfd.as_raw_fd() as libc::id_t, &mut info, libc::WEXITED) };
    if rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD) { return Ok(()); }
    Err(format!("reap worker root by pidfd: {}", std::io::Error::last_os_error()))
}

fn cleanup_failed_launch(
    kill_fd: RawFd,
    leaf: &Path,
    pid: libc::pid_t,
    pidfd: Option<&OwnedFd>,
    error: String,
) -> String {
    let kill = write_all_fd(kill_fd, b"1");
    let empty = wait_recursive_empty(leaf, EMPTY_DEADLINE);
    let reap = if let Some(pidfd) = pidfd {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let rc = unsafe {
            libc::waitid(
                P_PIDFD,
                pidfd.as_raw_fd() as libc::id_t,
                &mut info,
                libc::WEXITED,
            )
        };
        if rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD) {
            Ok(())
        } else {
            Err(format!(
                "waitid failed launch root: {}",
                std::io::Error::last_os_error()
            ))
        }
    } else {
        let mut status = 0;
        if unsafe { libc::waitpid(pid, &mut status, 0) } == pid {
            Ok(())
        } else {
            Err(format!(
                "waitpid failed launch root: {}",
                std::io::Error::last_os_error()
            ))
        }
    };
    let cleanup = [kill.err(), empty.err(), reap.err()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    if cleanup.is_empty() {
        error
    } else {
        format!("{error}; launch cleanup failed: {}", cleanup.join("; "))
    }
}

fn pipe_cloexec() -> Result<(OwnedFd, OwnedFd), String> {
    let mut fds = [-1; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 { return Err(format!("create custody pipe: {}", std::io::Error::last_os_error())); }
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

fn open_fd(path: &Path, flags: i32) -> Result<OwnedFd, String> {
    let path_c = c_path(path)?;
    let fd = unsafe { libc::open(path_c.as_ptr(), flags | libc::O_CLOEXEC | libc::O_NOFOLLOW) };
    if fd < 0 { return Err(format!("open {}: {}", path.display(), std::io::Error::last_os_error())); }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn openat_fd(directory: RawFd, name: &str, flags: i32) -> Result<OwnedFd, String> {
    let name = CString::new(name)
        .map_err(|_| "cgroup control filename contains NUL".to_string())?;
    let fd = unsafe {
        libc::openat(
            directory,
            name.as_ptr(),
            flags | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        return Err(format!(
            "open bootstrap cgroup control: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn dup_cloexec(fd: RawFd) -> Result<OwnedFd, String> {
    let duplicated = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
    if duplicated < 0 { return Err(format!("duplicate custody FD: {}", std::io::Error::last_os_error())); }
    Ok(unsafe { OwnedFd::from_raw_fd(duplicated) })
}

fn set_cloexec(fd: RawFd) -> Result<(), String> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } != 0 { return Err(format!("set FD_CLOEXEC: {}", std::io::Error::last_os_error())); }
    Ok(())
}

fn close_unlisted_fds(keep: &[RawFd]) -> Result<(), std::io::Error> {
    let mut first = 3_u32;
    for kept in keep.iter().copied().filter(|fd| *fd >= 3) {
        let kept = kept as u32;
        if first < kept {
            close_range(first, kept - 1)?;
        }
        first = kept
            .checked_add(1)
            .ok_or_else(|| std::io::Error::other("inherited FD overflow"))?;
    }
    close_range(first, u32::MAX)
}

fn close_range(first: u32, last: u32) -> Result<(), std::io::Error> {
    if first > last {
        return Ok(());
    }
    if unsafe { libc::syscall(libc::SYS_close_range, first, last, 0) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn write_all_fd(fd: RawFd, mut bytes: &[u8]) -> Result<(), String> {
    while !bytes.is_empty() {
        let count = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        if count < 0 { let e = std::io::Error::last_os_error(); if e.kind() == std::io::ErrorKind::Interrupted { continue; } return Err(format!("write custody FD: {e}")); }
        if count == 0 { return Err("write custody FD made no progress".to_string()); }
        bytes = &bytes[count as usize..];
    }
    Ok(())
}

fn read_one_with_deadline(fd: RawFd, deadline: Duration) -> Result<u8, String> {
    let mut byte = [0];
    if read_with_deadline(fd, &mut byte, deadline)? != 1 { return Err("custody prepared pipe closed before byte".to_string()); }
    Ok(byte[0])
}

fn read_with_deadline(fd: RawFd, bytes: &mut [u8], deadline: Duration) -> Result<usize, String> {
    let mut poll = libc::pollfd { fd, events: libc::POLLIN | libc::POLLHUP | libc::POLLERR, revents: 0 };
    let rc = unsafe { libc::poll(&mut poll, 1, deadline.as_millis().min(i32::MAX as u128) as i32) };
    if rc == 0 { return Err("custody pipe deadline elapsed".to_string()); }
    if rc < 0 { return Err(format!("poll custody pipe: {}", std::io::Error::last_os_error())); }
    let count = unsafe { libc::read(fd, bytes.as_mut_ptr().cast(), bytes.len()) };
    if count < 0 { return Err(format!("read custody pipe: {}", std::io::Error::last_os_error())); }
    Ok(count as usize)
}

fn c_path(path: &Path) -> Result<CString, String> { CString::new(path.as_os_str().as_bytes()).map_err(|_| format!("path contains NUL: {}", path.display())) }
fn c_argv(program: &Path, arguments: &[String]) -> Result<Vec<CString>, String> {
    std::iter::once(program.as_os_str()).chain(arguments.iter().map(OsStr::new)).map(|v| CString::new(v.as_bytes()).map_err(|_| "worker argument contains NUL".to_string())).collect()
}
fn c_env(environment: &BTreeMap<String, String>) -> Result<Vec<CString>, String> {
    environment.iter().map(|(k, v)| CString::new(format!("{k}={v}")).map_err(|_| "worker environment contains NUL".to_string())).collect()
}
fn fstat_mode(fd: RawFd) -> Result<libc::mode_t, String> { let mut stat: libc::stat = unsafe { std::mem::zeroed() }; if unsafe { libc::fstat(fd, &mut stat) } != 0 { return Err(format!("fstat custody FD: {}", std::io::Error::last_os_error())); } Ok(stat.st_mode) }
fn fstat_metadata(fd: RawFd) -> Result<(u64, u64), String> { let mut stat: libc::stat = unsafe { std::mem::zeroed() }; if unsafe { libc::fstat(fd, &mut stat) } != 0 { return Err(format!("fstat lease probe: {}", std::io::Error::last_os_error())); } Ok((stat.st_dev, stat.st_ino)) }

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn tool_code_stays_stopped_until_activation_is_acknowledged() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let workspace = std::env::temp_dir().join(format!(
            "session-relay-activation-{}-{suffix}",
            unsafe { libc::getpid() }
        ));
        fs::create_dir(&workspace).unwrap();
        let marker = workspace.join("tool-ran");
        let command = format!("printf activated > {}", marker.display());
        let argv = c_argv(
            Path::new("/bin/sh"),
            &["-c".to_string(), command],
        )
        .unwrap();
        let env = c_env(&BTreeMap::new()).unwrap();
        let cwd = c_path(&workspace).unwrap();
        let policy = LandlockPolicy {
            workspace: workspace.clone(),
            readable: Vec::new(),
            writable_resources: Vec::new(),
        };
        let (activation_read, activation_write) = pipe_cloexec().unwrap();
        let (prepared_read, prepared_write) = pipe_cloexec().unwrap();
        let (exec_read, exec_write) = pipe_cloexec().unwrap();

        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            drop(activation_write);
            drop(prepared_read);
            drop(exec_read);
            let result = child_prepare_and_exec(
                &argv,
                &env,
                &cwd,
                &policy,
                &[],
                activation_read.as_raw_fd(),
                prepared_write.as_raw_fd(),
                exec_write.as_raw_fd(),
            );
            let errno = result
                .err()
                .and_then(|error| error.raw_os_error())
                .unwrap_or(libc::EPERM);
            unsafe {
                libc::write(
                    exec_write.as_raw_fd(),
                    errno.to_ne_bytes().as_ptr().cast(),
                    std::mem::size_of::<i32>(),
                );
                libc::_exit(127);
            }
        }

        drop(activation_read);
        drop(prepared_write);
        drop(exec_write);
        assert_eq!(
            read_one_with_deadline(prepared_read.as_raw_fd(), PREPARED_DEADLINE)
                .unwrap(),
            0x01
        );
        write_all_fd(activation_write.as_raw_fd(), &[0x01]).unwrap();
        drop(activation_write);
        let mut exec_status = [0_u8; 4];
        assert_eq!(
            read_with_deadline(
                exec_read.as_raw_fd(),
                &mut exec_status,
                PREPARED_DEADLINE,
            )
            .unwrap(),
            0
        );

        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(pid, &mut status, libc::WUNTRACED) }, pid);
        let stopped_before_ack = libc::WIFSTOPPED(status);
        let ran_before_ack = marker.exists();
        if stopped_before_ack {
            assert_eq!(
                unsafe {
                    libc::ptrace(
                        libc::PTRACE_DETACH,
                        pid,
                        std::ptr::null_mut::<libc::c_void>(),
                        std::ptr::null_mut::<libc::c_void>(),
                    )
                },
                0
            );
            assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
        }

        let ran_after_ack = marker.exists();
        let _ = fs::remove_file(&marker);
        fs::remove_dir(&workspace).unwrap();
        assert!(stopped_before_ack, "worker was not stopped at traced exec");
        assert!(!ran_before_ack, "tool code ran before ACTIVATED acknowledgment");
        assert!(ran_after_ack, "tool code did not run after activation acknowledgment");
    }

    #[test]
    fn pidfd_echild_rejects_a_zombie_root() {
        let (pid_read, pid_write) = pipe_cloexec().unwrap();
        let (release_read, release_write) = pipe_cloexec().unwrap();
        let intermediary = unsafe { libc::fork() };
        assert!(intermediary >= 0);
        if intermediary == 0 {
            drop(pid_read);
            drop(release_write);
            let child = unsafe { libc::fork() };
            if child == 0 {
                unsafe { libc::_exit(0) };
            }
            write_all_fd(
                pid_write.as_raw_fd(),
                &child.to_ne_bytes(),
            )
            .unwrap();
            drop(pid_write);
            let _ = read_one_with_deadline(
                release_read.as_raw_fd(),
                PREPARED_DEADLINE,
            );
            let mut status = 0;
            unsafe {
                libc::waitpid(child, &mut status, 0);
                libc::_exit(0);
            }
        }

        drop(pid_write);
        drop(release_read);
        let mut pid_bytes = [0_u8; std::mem::size_of::<libc::pid_t>()];
        assert_eq!(
            read_with_deadline(
                pid_read.as_raw_fd(),
                &mut pid_bytes,
                PREPARED_DEADLINE,
            )
            .unwrap(),
            pid_bytes.len()
        );
        let pid = libc::pid_t::from_ne_bytes(pid_bytes);
        let deadline = Instant::now() + PREPARED_DEADLINE;
        let start_token = loop {
            let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
            let end = stat.rfind(") ").unwrap();
            if stat[end + 2..].starts_with('Z') {
                break process_start_token(pid).unwrap();
            }
            assert!(Instant::now() < deadline, "child did not become a zombie");
            std::thread::sleep(Duration::from_millis(1));
        };
        let identity = ProcessIdentity {
            pid,
            pidfd: pidfd_open(pid).unwrap(),
            start_token,
        };
        let accepted_prepared_identity = validate_pidfd_identity(
            identity.pidfd.as_raw_fd(),
            identity.pid,
            &identity.start_token,
        );
        let live = pidfd_is_live(&identity).unwrap();

        write_all_fd(release_write.as_raw_fd(), &[0x01]).unwrap();
        drop(release_write);
        let mut status = 0;
        assert_eq!(
            unsafe { libc::waitpid(intermediary, &mut status, 0) },
            intermediary
        );
        assert!(
            accepted_prepared_identity.is_err(),
            "WORKER_PREPARED accepted a zombie pidfd identity"
        );
        assert!(!live, "zombie root was accepted by start-token liveness");
    }
}
