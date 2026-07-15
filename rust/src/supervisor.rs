//! Detached lifecycle child custody and bounded stdio proxying.
//!
//! The caller supplies only a sealed lifecycle guard plus a closed launch
//! variant. A detached watchdog owns the supervisor process; the supervisor
//! resolves all executable/session/cwd authority from the durable operation
//! record and owns the runtime `Child` from birth through reap.

use crate::lifecycle::{
    ChildLaunchSpec, LifecycleStore, PreparedSupervisorLaunch, ProcessObservation, ReentryGuard,
    ResolvedSupervisorLaunch, StartGeneration, StdioEndpointMode, StdioProfile, SupervisorRecord,
    SupervisorState,
};
use crate::sha256::hex_digest;
use crate::spawn::CHILD_ENV_ALLOWLIST;
use crate::store;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, IsTerminal, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::*;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, ExitStatus, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tinyjson::JsonValue;

const FRAME_BYTES: usize = 64 * 1024;
const QUEUE_FRAMES: usize = (1024 * 1024) / FRAME_BYTES;
const READY_POLL: Duration = Duration::from_millis(20);
const CONTROL_POLL: Duration = Duration::from_millis(50);
const CONTROL_BIND_DEADLINE: Duration = Duration::from_millis(5_000);
const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SupervisorStartupPhase {
    WatchdogBootstrap,
    SupervisorBootstrap,
    ListenerBound,
    GlobalQueuesReady,
    AcceptorsReady,
    ControlAuthenticated,
    OperationQueuesReady,
    ChildSpawned,
    ProxyThreadsReady,
    StartedPublished,
}

impl SupervisorStartupPhase {
    pub const ALL: [Self; 10] = [
        Self::WatchdogBootstrap,
        Self::SupervisorBootstrap,
        Self::ListenerBound,
        Self::GlobalQueuesReady,
        Self::AcceptorsReady,
        Self::ControlAuthenticated,
        Self::OperationQueuesReady,
        Self::ChildSpawned,
        Self::ProxyThreadsReady,
        Self::StartedPublished,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WatchdogBootstrap => "WatchdogBootstrap",
            Self::SupervisorBootstrap => "SupervisorBootstrap",
            Self::ListenerBound => "ListenerBound",
            Self::GlobalQueuesReady => "GlobalQueuesReady",
            Self::AcceptorsReady => "AcceptorsReady",
            Self::ControlAuthenticated => "ControlAuthenticated",
            Self::OperationQueuesReady => "OperationQueuesReady",
            Self::ChildSpawned => "ChildSpawned",
            Self::ProxyThreadsReady => "ProxyThreadsReady",
            Self::StartedPublished => "StartedPublished",
        }
    }
}

fn startup_checkpoint(phase: SupervisorStartupPhase, edge: &str) -> Result<(), String> {
    let expected = format!("{edge}:{}", phase.as_str());
    if std::env::var("RELAY_TEST_SUPERVISOR_STARTUP_FAIL").as_deref() == Ok(expected.as_str()) {
        return Err(format!(
            "injected supervisor startup failure {edge}:{}",
            phase.as_str()
        ));
    }
    Ok(())
}

pub fn run_child_with_guard(
    guard: &mut ReentryGuard,
    spec: ChildLaunchSpec,
) -> Result<Output, String> {
    let profile = stdio_profile(&spec);
    let mut prepared = guard.prepare_supervisor_launch(spec, profile.clone())?;
    let bootstrap = spawn_watchdog(&prepared)?;
    prepared.control_epoch = bootstrap.control_epoch.clone();
    let mut stream = connect_control(&prepared, &bootstrap)?;
    verify_same_uid(&stream)?;
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .map_err(|error| format!("configure supervisor read timeout: {error}"))?;
    let writer = Arc::new(Mutex::new(
        stream
            .try_clone()
            .map_err(|error| format!("clone supervisor socket: {error}"))?,
    ));
    let _terminal_mode = profile_is_pty(&profile)
        .then(TerminalModeGuard::enter_raw)
        .transpose()?;
    let start_gate = Arc::new(AtomicBool::new(false));
    start_input_proxy(&profile, Arc::clone(&writer), Arc::clone(&start_gate))?;
    start_resize_proxy(&profile, Arc::clone(&writer), Arc::clone(&start_gate))?;
    send_simple(&writer, "control_go")?;
    wait_control_ready(&mut stream, &prepared)?;
    start_gate.store(true, Ordering::Release);

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut reader = BufReader::new(&mut stream);
    let mut cancel_sent = false;
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => return Err("lifecycle supervisor disconnected before reap".to_string()),
            Ok(_) => {
                let frame = parse_frame(&line)?;
                match frame_string(&frame, "kind")?.as_str() {
                    "output" => {
                        let bytes = decode_hex(&frame_string(&frame, "bytes_hex")?)?;
                        let stream_name = frame_string(&frame, "stream")?;
                        if profile_is_pty(&profile) {
                            if stream_name == "stderr" {
                                std::io::stderr()
                                    .write_all(&bytes)
                                    .map_err(|error| format!("write supervised stderr: {error}"))?;
                                std::io::stderr().flush().ok();
                            } else {
                                std::io::stdout()
                                    .write_all(&bytes)
                                    .map_err(|error| format!("write supervised stdout: {error}"))?;
                                std::io::stdout().flush().ok();
                            }
                        } else if stream_name == "stderr" {
                            stderr.extend_from_slice(&bytes);
                        } else {
                            stdout.extend_from_slice(&bytes);
                        }
                    }
                    "reaped" => {
                        let (code, status) = reaped_status(&frame)?;
                        if cancel_sent || guard.cancelled() {
                            return Err(format!("guarded child cancelled after status {code}"));
                        }
                        return Ok(Output {
                            status,
                            stdout,
                            stderr,
                        });
                    }
                    "lost" => {
                        return Err(format!(
                            "lifecycle supervisor lost authority: {}",
                            frame_string(&frame, "reason")?
                        ));
                    }
                    other => return Err(format!("unknown supervisor event: {other}")),
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                if guard.cancelled() && !cancel_sent {
                    send_simple(&writer, "cancel")?;
                    cancel_sent = true;
                }
            }
            Err(error) => return Err(format!("read lifecycle supervisor event: {error}")),
        }
    }
}

struct TerminalModeGuard {
    fd: i32,
    original: libc::termios,
}

impl TerminalModeGuard {
    fn enter_raw() -> Result<Self, String> {
        let fd = std::io::stdin().as_raw_fd();
        // SAFETY: termios is initialized by tcgetattr before it is read.
        let mut original = unsafe { std::mem::zeroed::<libc::termios>() };
        // SAFETY: fd is live and original is a valid output buffer.
        if unsafe { libc::tcgetattr(fd, &mut original) } != 0 {
            return Err(format!(
                "read caller terminal mode: {}",
                std::io::Error::last_os_error()
            ));
        }
        let mut raw = original;
        // SAFETY: raw is an initialized termios value.
        unsafe { libc::cfmakeraw(&mut raw) };
        // SAFETY: fd is live and raw is a valid termios value.
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            return Err(format!(
                "set caller terminal raw mode: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(Self { fd, original })
    }
}

impl Drop for TerminalModeGuard {
    fn drop(&mut self) {
        // SAFETY: fd remains the process stdin and original came from tcgetattr.
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.original);
        }
    }
}

pub fn run_watchdog(argv: &[String]) -> Result<(), String> {
    let args = HiddenArgs::parse("__lifecycle-watchdog", argv)?;
    startup_checkpoint(SupervisorStartupPhase::WatchdogBootstrap, "before")?;
    let mut caller_bootstrap = inherited_file(args.bootstrap_fd)?;
    let store = LifecycleStore::new(args.root.clone());
    let watchdog_instance_id = store::uuid_v4();
    let control_epoch = store::uuid_v4();
    let control_nonce = random_hex(32)?;
    let control_nonce_sha256 = hex_digest(control_nonce.as_bytes());
    store.record_watchdog(
        &watchdog_instance_id,
        &args.supervisor_instance_id,
        &args.operation_id,
        &control_epoch,
        &control_nonce_sha256,
        &observe_process(std::process::id()),
    )?;
    startup_checkpoint(SupervisorStartupPhase::WatchdogBootstrap, "after")?;
    let (supervisor_nonce_read, mut supervisor_nonce_write) = cloexec_pipe()?;
    let executable = std::env::current_exe()
        .map_err(|error| format!("resolve relay executable for supervisor: {error}"))?;
    let supervisor_fd = supervisor_nonce_read.as_raw_fd();
    let supervisor_args = HiddenArgs {
        control_epoch: control_epoch.clone(),
        bootstrap_fd: supervisor_fd,
        ..args.clone()
    };
    let mut command = Command::new(executable);
    command
        .arg("__lifecycle-supervisor")
        .args(supervisor_args.to_argv())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    prepare_detached_child(&mut command, supervisor_fd);
    let mut child = command
        .spawn()
        .map_err(|error| format!("spawn lifecycle supervisor: {error}"))?;
    drop(supervisor_nonce_read);
    writeln!(supervisor_nonce_write, "{control_nonce}")
        .map_err(|error| format!("write supervisor nonce bootstrap: {error}"))?;
    supervisor_nonce_write
        .flush()
        .map_err(|error| format!("flush supervisor nonce bootstrap: {error}"))?;
    drop(supervisor_nonce_write);
    let response = format!(
        "{{\"control_epoch\":\"{control_epoch}\",\"control_nonce\":\"{control_nonce}\",\"kind\":\"watchdog_bootstrap\",\"supervisor_instance_id\":\"{}\"}}\n",
        args.supervisor_instance_id
    );
    caller_bootstrap
        .write_all(response.as_bytes())
        .map_err(|error| format!("write caller watchdog bootstrap: {error}"))?;
    caller_bootstrap
        .flush()
        .map_err(|error| format!("flush caller watchdog bootstrap: {error}"))?;
    drop(caller_bootstrap);

    let status = loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("poll lifecycle supervisor: {error}"))?
        {
            break status;
        }
        store.heartbeat_watchdog(
            &watchdog_instance_id,
            &args.supervisor_instance_id,
            &control_epoch,
        )?;
        thread::sleep(HEARTBEAT_INTERVAL);
    };
    if status.success() {
        store.finish_supervisor_service(&args.supervisor_instance_id, &watchdog_instance_id)
    } else {
        store.mark_supervisor_lost(
            &args.supervisor_instance_id,
            crate::lifecycle::LostAuthorityReason::SupervisorLost,
        )?;
        store.finish_watchdog_service(&watchdog_instance_id)
    }
}

pub fn run_supervisor(argv: &[String]) -> Result<(), String> {
    let args = HiddenArgs::parse("__lifecycle-supervisor", argv)?;
    startup_checkpoint(SupervisorStartupPhase::SupervisorBootstrap, "before")?;
    let control_nonce =
        read_bootstrap_line(inherited_file(args.bootstrap_fd)?, CONTROL_BIND_DEADLINE)?;
    if control_nonce.is_empty() {
        return Err("supervisor control nonce bootstrap is empty".to_string());
    }
    let store = LifecycleStore::new(args.root.clone());
    store.validate_watchdog_bootstrap(
        &args.supervisor_instance_id,
        &args.control_epoch,
        &hex_digest(control_nonce.as_bytes()),
    )?;
    startup_checkpoint(SupervisorStartupPhase::SupervisorBootstrap, "after")?;
    let resolved = store.resolve_supervisor_launch(
        &args.operation_id,
        &args.operation_version,
        &args.supervisor_instance_id,
    )?;
    let socket_path = supervisor_socket_path(&args.operation_id);
    if socket_path.exists() {
        fs::remove_file(&socket_path)
            .map_err(|error| format!("remove stale supervisor socket: {error}"))?;
    }
    startup_checkpoint(SupervisorStartupPhase::ListenerBound, "before")?;
    let listener = UnixListener::bind(&socket_path)
        .map_err(|error| format!("bind lifecycle supervisor socket: {error}"))?;
    fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("chmod lifecycle supervisor socket: {error}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("configure lifecycle supervisor listener: {error}"))?;
    startup_checkpoint(SupervisorStartupPhase::ListenerBound, "after")?;
    startup_checkpoint(SupervisorStartupPhase::GlobalQueuesReady, "before")?;
    startup_checkpoint(SupervisorStartupPhase::GlobalQueuesReady, "after")?;
    let accept_deadline = Instant::now() + CONTROL_BIND_DEADLINE;
    let mut stream = loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                if verify_same_uid(&stream).is_err() {
                    continue;
                }
                stream
                    .set_read_timeout(Some(Duration::from_millis(100)))
                    .map_err(|error| format!("configure initial supervisor socket: {error}"))?;
                let Ok(frame) = read_frame_unbuffered(&mut stream) else {
                    continue;
                };
                match frame_string(&frame, "role").as_deref() {
                    Ok("health") if frame_string(&frame, "kind").as_deref() == Ok("ping") => {
                        let challenge = frame_string(&frame, "challenge_nonce")?;
                        write_health_response(
                            &mut stream,
                            &args.supervisor_instance_id,
                            &challenge,
                        )?;
                        continue;
                    }
                    Ok("control")
                        if frame_string(&frame, "kind").as_deref() == Ok("hello")
                            && frame_string(&frame, "supervisor_instance_id").as_deref()
                                == Ok(args.supervisor_instance_id.as_str())
                            && frame_string(&frame, "operation_id").as_deref()
                                == Ok(args.operation_id.as_str())
                            && frame_string(&frame, "operation_version").as_deref()
                                == Ok(args.operation_version.as_str())
                            && frame_string(&frame, "control_epoch").as_deref()
                                == Ok(args.control_epoch.as_str())
                            && frame_string(&frame, "control_nonce").as_deref()
                                == Ok(control_nonce.as_str()) =>
                    {
                        startup_checkpoint(SupervisorStartupPhase::ControlAuthenticated, "before")?;
                        let mut response = identity_frame("control_authenticated", &args);
                        response.insert("role".into(), JsonValue::from("control".to_string()));
                        if write_frame(&mut stream, response).is_err() {
                            store.record_child_start_abandoned(
                                &args.operation_id,
                                &args.operation_version,
                                &args.supervisor_instance_id,
                                "caller disconnected during control authentication",
                            )?;
                            fs::remove_file(&socket_path).ok();
                            return Ok(());
                        }
                        startup_checkpoint(SupervisorStartupPhase::ControlAuthenticated, "after")?;
                        break stream;
                    }
                    _ => continue,
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if !pid_exists(args.caller_pid) || Instant::now() >= accept_deadline {
                    store.record_child_start_abandoned(
                        &args.operation_id,
                        &args.operation_version,
                        &args.supervisor_instance_id,
                        "caller disconnected before supervisor launch",
                    )?;
                    fs::remove_file(&socket_path).ok();
                    return Ok(());
                }
                thread::sleep(CONTROL_POLL);
            }
            Err(error) => return Err(format!("accept lifecycle supervisor caller: {error}")),
        }
    };
    listener
        .set_nonblocking(false)
        .map_err(|error| format!("restore lifecycle supervisor listener: {error}"))?;
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .map_err(|error| format!("configure control GO timeout: {error}"))?;
    let go = match read_frame_unbuffered(&mut stream) {
        Ok(go) => go,
        Err(_) => {
            store.record_child_start_abandoned(
                &args.operation_id,
                &args.operation_version,
                &args.supervisor_instance_id,
                "authenticated caller disconnected before control GO",
            )?;
            fs::remove_file(&socket_path).ok();
            return Ok(());
        }
    };
    if frame_string(&go, "kind").as_deref() != Ok("control_go") {
        store.record_child_start_abandoned(
            &args.operation_id,
            &args.operation_version,
            &args.supervisor_instance_id,
            "authenticated caller disconnected before control GO",
        )?;
        fs::remove_file(&socket_path).ok();
        return Ok(());
    }
    startup_checkpoint(SupervisorStartupPhase::OperationQueuesReady, "before")?;
    let (control_tx, control_rx) = mpsc::sync_channel(QUEUE_FRAMES);
    let control_stream = stream
        .try_clone()
        .map_err(|error| format!("clone lifecycle control socket: {error}"))?;
    start_control_reader(control_stream, control_tx)?;
    startup_checkpoint(SupervisorStartupPhase::AcceptorsReady, "before")?;
    start_health_listener(listener, args.supervisor_instance_id.clone())?;
    startup_checkpoint(SupervisorStartupPhase::AcceptorsReady, "after")?;
    startup_checkpoint(SupervisorStartupPhase::OperationQueuesReady, "after")?;
    let metadata = fs::metadata(&socket_path)
        .map_err(|error| format!("stat lifecycle supervisor socket: {error}"))?;
    store.record_supervisor_ready(SupervisorRecord {
        supervisor_instance_id: args.supervisor_instance_id.clone(),
        control_epoch: args.control_epoch.clone(),
        control_nonce_sha256: hex_digest(control_nonce.as_bytes()),
        operation_id: args.operation_id.clone(),
        process: observe_process(std::process::id()),
        socket_path: socket_path.clone(),
        socket_dev: metadata.dev(),
        socket_ino: metadata.ino(),
        version: "1".to_string(),
        state: SupervisorState::Ready,
        heartbeat_at: store::iso_now(),
        heartbeat_at_ms: store::now_ms(),
    })?;
    stream.set_read_timeout(None).ok();
    if write_frame(&mut stream, identity_frame("control_bound", &args)).is_err() {
        store.record_child_start_abandoned(
            &args.operation_id,
            &args.operation_version,
            &args.supervisor_instance_id,
            "caller disconnected before control bound",
        )?;
        fs::remove_file(&socket_path).ok();
        return Ok(());
    }
    let outcome = supervise_connected(&store, &args, resolved, stream, control_rx);
    fs::remove_file(socket_path).ok();
    outcome
}

fn start_health_listener(
    listener: UnixListener,
    supervisor_instance_id: String,
) -> Result<(), String> {
    spawn_named("health", "relay-supervisor-health", move || {
        for connection in listener.incoming() {
            let Ok(mut stream) = connection else { break };
            if verify_same_uid(&stream).is_err() {
                continue;
            }
            stream
                .set_read_timeout(Some(Duration::from_millis(100)))
                .ok();
            let Ok(frame) = read_frame_unbuffered(&mut stream) else {
                continue;
            };
            if frame_string(&frame, "role").as_deref() != Ok("health")
                || frame_string(&frame, "kind").as_deref() != Ok("ping")
            {
                continue;
            }
            let Ok(challenge) = frame_string(&frame, "challenge_nonce") else {
                continue;
            };
            let _ = write_health_response(&mut stream, &supervisor_instance_id, &challenge);
        }
    })
    .map(|_| ())
}

#[derive(Clone, Debug)]
struct HiddenArgs {
    root: PathBuf,
    operation_id: String,
    operation_version: String,
    supervisor_instance_id: String,
    caller_pid: u32,
    control_epoch: String,
    bootstrap_fd: RawFd,
}

impl HiddenArgs {
    fn parse(name: &str, argv: &[String]) -> Result<Self, String> {
        if argv.len() != 7 {
            return Err(format!(
                "usage: relay {name} <root> <operation-id> <operation-version> <supervisor-instance-id> <caller-pid> <control-epoch|-> <bootstrap-fd>"
            ));
        }
        Ok(Self {
            root: PathBuf::from(&argv[0]),
            operation_id: argv[1].clone(),
            operation_version: argv[2].clone(),
            supervisor_instance_id: argv[3].clone(),
            caller_pid: argv[4]
                .parse()
                .map_err(|_| "lifecycle caller pid is invalid".to_string())?,
            control_epoch: argv[5].clone(),
            bootstrap_fd: argv[6]
                .parse()
                .map_err(|_| "lifecycle bootstrap fd is invalid".to_string())?,
        })
    }

    fn to_argv(&self) -> [String; 7] {
        [
            self.root.to_string_lossy().into_owned(),
            self.operation_id.clone(),
            self.operation_version.clone(),
            self.supervisor_instance_id.clone(),
            self.caller_pid.to_string(),
            self.control_epoch.clone(),
            self.bootstrap_fd.to_string(),
        ]
    }
}

#[derive(Clone, Debug)]
struct WatchdogBootstrap {
    control_epoch: String,
    control_nonce: String,
}

fn spawn_watchdog(prepared: &PreparedSupervisorLaunch) -> Result<WatchdogBootstrap, String> {
    let executable = std::env::current_exe()
        .map_err(|error| format!("resolve relay executable for watchdog: {error}"))?;
    let (bootstrap_read, bootstrap_write) = cloexec_pipe()?;
    let bootstrap_fd = bootstrap_write.as_raw_fd();
    let args = HiddenArgs {
        root: prepared.root.clone(),
        operation_id: prepared.operation_id.clone(),
        operation_version: prepared.operation_version.clone(),
        supervisor_instance_id: prepared.supervisor_instance_id.clone(),
        caller_pid: std::process::id(),
        control_epoch: "-".to_string(),
        bootstrap_fd,
    };
    let mut command = Command::new(executable);
    command
        .arg("__lifecycle-watchdog")
        .args(args.to_argv())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    prepare_detached_child(&mut command, bootstrap_fd);
    let child = command
        .spawn()
        .map_err(|error| format!("spawn detached lifecycle watchdog: {error}"))?;
    drop(child);
    drop(bootstrap_write);
    let line = read_bootstrap_line(bootstrap_read, CONTROL_BIND_DEADLINE)?;
    let frame = parse_frame(&line)?;
    if frame_string(&frame, "kind").as_deref() != Ok("watchdog_bootstrap")
        || frame_string(&frame, "supervisor_instance_id").as_deref()
            != Ok(prepared.supervisor_instance_id.as_str())
    {
        return Err("watchdog bootstrap identity changed".to_string());
    }
    Ok(WatchdogBootstrap {
        control_epoch: frame_string(&frame, "control_epoch")?,
        control_nonce: frame_string(&frame, "control_nonce")?,
    })
}

fn connect_control(
    prepared: &PreparedSupervisorLaunch,
    bootstrap: &WatchdogBootstrap,
) -> Result<UnixStream, String> {
    let socket_path = supervisor_socket_path(&prepared.operation_id);
    let deadline =
        Instant::now() + Duration::from_millis(crate::lifecycle::MANAGED_ATTACH_DEADLINE_MS);
    loop {
        match UnixStream::connect(&socket_path) {
            Ok(mut stream) => {
                verify_same_uid(&stream)?;
                let args = HiddenArgs {
                    root: prepared.root.clone(),
                    operation_id: prepared.operation_id.clone(),
                    operation_version: prepared.operation_version.clone(),
                    supervisor_instance_id: prepared.supervisor_instance_id.clone(),
                    caller_pid: std::process::id(),
                    control_epoch: bootstrap.control_epoch.clone(),
                    bootstrap_fd: -1,
                };
                let mut hello = identity_frame("hello", &args);
                hello.insert("role".into(), JsonValue::from("control".to_string()));
                hello.insert(
                    "control_nonce".into(),
                    JsonValue::from(bootstrap.control_nonce.clone()),
                );
                write_frame(&mut stream, hello)?;
                stream
                    .set_read_timeout(Some(Duration::from_millis(500)))
                    .map_err(|error| {
                        format!("configure control authentication timeout: {error}")
                    })?;
                let response = read_frame_unbuffered(&mut stream)?;
                validate_identity_frame(&response, "control_authenticated", prepared)?;
                return Ok(stream);
            }
            Err(error) if Instant::now() < deadline => {
                let _ = error;
                thread::sleep(READY_POLL);
            }
            Err(error) => return Err(format!("connect lifecycle supervisor socket: {error}")),
        }
    }
}

fn wait_control_ready(
    stream: &mut UnixStream,
    prepared: &PreparedSupervisorLaunch,
) -> Result<(), String> {
    let response = read_frame_unbuffered(stream)?;
    validate_identity_frame(&response, "control_bound", prepared)?;
    let metadata = fs::metadata(supervisor_socket_path(&prepared.operation_id))
        .map_err(|error| format!("stat lifecycle supervisor socket: {error}"))?;
    let record = LifecycleStore::new(prepared.root.clone())
        .read_supervisor_for_session_from_operation(&prepared.operation_id)?
        .ok_or_else(|| "lifecycle supervisor ready record is missing".to_string())?;
    if record.supervisor_instance_id != prepared.supervisor_instance_id
        || record.socket_dev != metadata.dev()
        || record.socket_ino != metadata.ino()
        || record.state != SupervisorState::Ready
        || record.control_epoch != prepared.control_epoch
    {
        return Err("lifecycle supervisor socket identity changed".to_string());
    }
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .ok();
    Ok(())
}

fn supervisor_socket_path(operation_id: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "relay-lifecycle-{}.sock",
        store::sanitize(operation_id)
    ))
}

fn stdio_profile(spec: &ChildLaunchSpec) -> StdioProfile {
    if !matches!(spec, ChildLaunchSpec::AttachResume(_)) {
        return StdioProfile {
            stdin: StdioEndpointMode::Closed,
            stdout: StdioEndpointMode::Pipe,
            stderr: StdioEndpointMode::Pipe,
        };
    }
    if std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
        && std::io::stderr().is_terminal()
    {
        let (rows, cols) = terminal_size(std::io::stdout().as_raw_fd()).unwrap_or((24, 80));
        let terminal = StdioEndpointMode::Pty {
            terminal_group: "stdio".to_string(),
            rows,
            cols,
        };
        StdioProfile {
            stdin: terminal.clone(),
            stdout: terminal.clone(),
            stderr: terminal,
        }
    } else {
        StdioProfile {
            stdin: StdioEndpointMode::Pipe,
            stdout: StdioEndpointMode::Pipe,
            stderr: StdioEndpointMode::Pipe,
        }
    }
}

fn profile_is_pty(profile: &StdioProfile) -> bool {
    matches!(profile.stdout, StdioEndpointMode::Pty { .. })
}

fn start_input_proxy(
    profile: &StdioProfile,
    writer: Arc<Mutex<UnixStream>>,
    start_gate: Arc<AtomicBool>,
) -> Result<(), String> {
    if matches!(profile.stdin, StdioEndpointMode::Closed) {
        return Ok(());
    }
    spawn_named("input", "relay-supervisor-input", move || {
        while !start_gate.load(Ordering::Acquire) {
            thread::yield_now();
        }
        let mut stdin = std::io::stdin().lock();
        let mut bytes = vec![0_u8; FRAME_BYTES];
        loop {
            match stdin.read(&mut bytes) {
                Ok(0) => {
                    let _ = send_simple(&writer, "input_eof");
                    break;
                }
                Ok(count) => {
                    let mut frame = HashMap::new();
                    frame.insert("kind".into(), JsonValue::from("input".to_string()));
                    frame.insert(
                        "bytes_hex".into(),
                        JsonValue::from(encode_hex(&bytes[..count])),
                    );
                    if write_frame_locked(&writer, frame).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    })
    .map(|_| ())
}

fn start_resize_proxy(
    profile: &StdioProfile,
    writer: Arc<Mutex<UnixStream>>,
    start_gate: Arc<AtomicBool>,
) -> Result<(), String> {
    let StdioEndpointMode::Pty {
        rows: initial_rows,
        cols: initial_cols,
        ..
    } = profile.stdout
    else {
        return Ok(());
    };
    spawn_named("resize", "relay-supervisor-resize", move || {
        while !start_gate.load(Ordering::Acquire) {
            thread::yield_now();
        }
        let mut last = (initial_rows, initial_cols);
        loop {
            thread::sleep(Duration::from_millis(100));
            let Ok(current) = terminal_size(std::io::stdout().as_raw_fd()) else {
                continue;
            };
            if current == last {
                continue;
            }
            let mut frame = HashMap::new();
            frame.insert("kind".into(), JsonValue::from("resize".to_string()));
            frame.insert("rows".into(), JsonValue::from(current.0.to_string()));
            frame.insert("cols".into(), JsonValue::from(current.1.to_string()));
            if write_frame_locked(&writer, frame).is_err() {
                break;
            }
            last = current;
        }
    })
    .map(|_| ())
}

enum Control {
    Input(Vec<u8>),
    InputEof,
    Cancel,
    Resize(u16, u16),
    Signal(i32),
    Disconnected,
}

enum OutputChunk {
    Bytes(&'static str, Vec<u8>),
    Done,
}

fn supervise_connected(
    store: &LifecycleStore,
    args: &HiddenArgs,
    resolved: ResolvedSupervisorLaunch,
    mut event_stream: UnixStream,
    control_rx: Receiver<Control>,
) -> Result<(), String> {
    startup_checkpoint(SupervisorStartupPhase::ChildSpawned, "before")?;
    let (mut child, input, outputs) = match spawn_owned_child(&resolved) {
        Ok(child) => child,
        Err(error) => {
            store.record_child_start_abandoned(
                &args.operation_id,
                &args.operation_version,
                &args.supervisor_instance_id,
                &error,
            )?;
            return Err(error);
        }
    };
    if let Err(error) = startup_checkpoint(SupervisorStartupPhase::ChildSpawned, "after") {
        reap_unpublished_child(store, args, &mut child, &error)?;
        return Err(error);
    }
    let process = observe_process(child.id());
    if let Err(error) = startup_checkpoint(SupervisorStartupPhase::ProxyThreadsReady, "before") {
        reap_unpublished_child(store, args, &mut child, &error)?;
        return Err(error);
    }
    let (output_tx, output_rx) = mpsc::sync_channel(QUEUE_FRAMES);
    let output_count = match start_output_readers(outputs, output_tx) {
        Ok(count) => count,
        Err(error) => {
            reap_unpublished_child(store, args, &mut child, &error)?;
            return Err(error);
        }
    };
    if let Err(error) = startup_checkpoint(SupervisorStartupPhase::ProxyThreadsReady, "after") {
        reap_unpublished_child(store, args, &mut child, &error)?;
        return Err(error);
    }
    if let Err(error) = startup_checkpoint(SupervisorStartupPhase::StartedPublished, "before") {
        reap_unpublished_child(store, args, &mut child, &error)?;
        return Err(error);
    }
    let mut operation_version = store.record_child_owned(
        &args.operation_id,
        &args.operation_version,
        &args.supervisor_instance_id,
        process.clone(),
    )?;
    if let Err(error) = startup_checkpoint(SupervisorStartupPhase::StartedPublished, "after") {
        cancel_and_reap_after_internal_failure(
            store,
            args,
            &mut child,
            &mut operation_version,
            &error,
        )?;
        return Err(error);
    }
    let mut input = input;
    let mut done_readers = 0;
    let mut disconnected = false;
    let mut cancellation_requested = false;
    let mut exit = None;
    let mut last_heartbeat = Instant::now();
    loop {
        if last_heartbeat.elapsed() >= HEARTBEAT_INTERVAL {
            store.heartbeat_supervisor(&args.supervisor_instance_id, &args.operation_id)?;
            last_heartbeat = Instant::now();
        }
        while let Ok(control) = control_rx.try_recv() {
            match control {
                Control::Input(bytes) => write_child_input(&mut input, &bytes)?,
                Control::InputEof => input = None,
                Control::Resize(rows, cols) => resize_child_input(&mut input, rows, cols)?,
                Control::Signal(signal) => {
                    if !matches!(signal, libc::SIGINT | libc::SIGTERM | libc::SIGHUP) {
                        continue;
                    }
                    signal_owned_child(&child, signal)?;
                }
                Control::Cancel | Control::Disconnected => {
                    disconnected = matches!(control, Control::Disconnected);
                    if !cancellation_requested {
                        store.publish_disconnect_cancel(
                            &args.operation_id,
                            &operation_version,
                            &args.supervisor_instance_id,
                        )?;
                        cancellation_requested = true;
                        if let Some(delay) =
                            std::env::var("RELAY_TEST_SUPERVISOR_CANCEL_BARRIER_MS")
                                .ok()
                                .and_then(|value| value.parse::<u64>().ok())
                        {
                            thread::sleep(Duration::from_millis(delay.min(5_000)));
                        }
                        operation_version = store.refresh_supervisor_operation_version(
                            &args.operation_id,
                            &args.supervisor_instance_id,
                        )?;
                        let permit = store.mint_child_cancellation_permit(
                            &args.operation_id,
                            &operation_version,
                            &args.supervisor_instance_id,
                            u64::from(child.id()),
                        )?;
                        store.authorize_child_cancellation(&permit)?;
                        child
                            .kill()
                            .map_err(|error| format!("kill owned supervised child: {error}"))?;
                    }
                }
            }
        }
        loop {
            match output_rx.try_recv() {
                Ok(OutputChunk::Bytes(stream, bytes)) => {
                    if !disconnected {
                        let mut frame = HashMap::new();
                        frame.insert("kind".into(), JsonValue::from("output".to_string()));
                        frame.insert("stream".into(), JsonValue::from(stream.to_string()));
                        frame.insert("bytes_hex".into(), JsonValue::from(encode_hex(&bytes)));
                        if write_frame(&mut event_stream, frame).is_err() {
                            disconnected = true;
                        }
                    }
                }
                Ok(OutputChunk::Done) => done_readers += 1,
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        if exit.is_none() {
            exit = child
                .try_wait()
                .map_err(|error| format!("poll owned supervised child: {error}"))?;
        }
        if exit.is_some() && done_readers >= output_count {
            break;
        }
        thread::sleep(CONTROL_POLL);
    }
    let status = exit.expect("child exit checked before break");
    let exit_code = normalized_exit_status(&status);
    let wait_status_raw = status.into_raw();
    operation_version = store
        .refresh_supervisor_operation_version(&args.operation_id, &args.supervisor_instance_id)?;
    store.record_child_reaped(
        &args.operation_id,
        &operation_version,
        &args.supervisor_instance_id,
        exit_code,
    )?;
    if !disconnected {
        let mut frame = HashMap::new();
        frame.insert("kind".into(), JsonValue::from("reaped".to_string()));
        frame.insert("exit_status".into(), JsonValue::from(exit_code.to_string()));
        frame.insert(
            "wait_status_raw".into(),
            JsonValue::from(wait_status_raw.to_string()),
        );
        write_frame(&mut event_stream, frame)?;
    }
    Ok(())
}

fn reap_unpublished_child(
    store: &LifecycleStore,
    args: &HiddenArgs,
    child: &mut Child,
    reason: &str,
) -> Result<(), String> {
    if child
        .try_wait()
        .map_err(|error| format!("observe unpublished child: {error}"))?
        .is_none()
    {
        child
            .kill()
            .map_err(|error| format!("kill unpublished supervised child: {error}"))?;
    }
    child
        .wait()
        .map_err(|error| format!("reap unpublished supervised child: {error}"))?;
    store.record_child_start_abandoned(
        &args.operation_id,
        &args.operation_version,
        &args.supervisor_instance_id,
        reason,
    )
}

enum ChildInput {
    Pipe(ChildStdin),
    Pty(File),
}

struct ChildOutputs {
    stdout: Option<Box<dyn Read + Send>>,
    stderr: Option<Box<dyn Read + Send>>,
}

fn spawn_owned_child(
    resolved: &ResolvedSupervisorLaunch,
) -> Result<(Child, Option<ChildInput>, ChildOutputs), String> {
    let (program, args, cwd) = derive_command(resolved)?;
    let mut command = Command::new(&program);
    command.args(args).env_clear();
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    for key in CHILD_ENV_ALLOWLIST {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    if profile_is_pty(&resolved.stdio) {
        let (rows, cols) = match resolved.stdio.stdin {
            StdioEndpointMode::Pty { rows, cols, .. } => (rows, cols),
            _ => return Err("PTY profile has non-PTY stdin".to_string()),
        };
        let (master, slave) = open_pty(rows, cols)?;
        let stdin = slave
            .try_clone()
            .map_err(|error| format!("clone PTY slave stdin: {error}"))?;
        let stdout = slave
            .try_clone()
            .map_err(|error| format!("clone PTY slave stdout: {error}"))?;
        command
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(slave));
        // SAFETY: only async-signal-safe libc calls execute between fork and exec.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command
            .spawn()
            .map_err(|error| format!("spawn supervised {program}: {error}"))?;
        let input = master
            .try_clone()
            .map(ChildInput::Pty)
            .map_err(|error| format!("clone PTY controller: {error}"))?;
        Ok((
            child,
            Some(input),
            ChildOutputs {
                stdout: Some(Box::new(master)),
                stderr: None,
            },
        ))
    } else {
        command
            .stdin(
                if matches!(resolved.stdio.stdin, StdioEndpointMode::Closed) {
                    Stdio::null()
                } else {
                    Stdio::piped()
                },
            )
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|error| format!("spawn supervised {program}: {error}"))?;
        let input = child.stdin.take().map(ChildInput::Pipe);
        let stdout = child
            .stdout
            .take()
            .map(|reader| Box::new(reader) as Box<dyn Read + Send>);
        let stderr = child
            .stderr
            .take()
            .map(|reader| Box::new(reader) as Box<dyn Read + Send>);
        Ok((child, input, ChildOutputs { stdout, stderr }))
    }
}

fn derive_command(
    resolved: &ResolvedSupervisorLaunch,
) -> Result<(String, Vec<String>, Option<String>), String> {
    match &resolved.spec {
        ChildLaunchSpec::AttachResume(options) => {
            let mut args = if resolved.tool == "codex" {
                if let Some(server) = resolved.server.as_deref() {
                    vec!["--remote".to_string(), format!("unix://{server}")]
                } else {
                    vec![
                        "resume".to_string(),
                        resolved.operation.runtime_session_id.clone(),
                        "-C".to_string(),
                        resolved.canonical_cwd.clone(),
                    ]
                }
            } else if resolved.tool == "claude" {
                vec![
                    "--resume".to_string(),
                    resolved.operation.runtime_session_id.clone(),
                ]
            } else {
                return Err(format!("unsupported supervised tool: {}", resolved.tool));
            };
            if let Some(model) = options.model() {
                args.push(
                    if resolved.tool == "codex" {
                        "-m"
                    } else {
                        "--model"
                    }
                    .to_string(),
                );
                args.push(model.to_string());
            }
            if let Some(effort) = options.effort() {
                if resolved.tool == "codex" {
                    args.extend(["-c".to_string(), format!("model_reasoning_effort={effort}")]);
                } else {
                    args.extend(["--effort".to_string(), effort.to_string()]);
                }
            }
            Ok((
                resolved.tool.clone(),
                args,
                (resolved.tool == "claude").then(|| resolved.canonical_cwd.clone()),
            ))
        }
        ChildLaunchSpec::WakeDoorbell(message) | ChildLaunchSpec::WatchWakeFallback(message) => {
            let program = wake_program(&resolved.tool)?;
            let mut args = if resolved.tool == "codex" {
                vec![
                    "exec".to_string(),
                    "resume".to_string(),
                    resolved.operation.runtime_session_id.clone(),
                    "--json".to_string(),
                ]
            } else if resolved.tool == "claude" {
                vec![
                    "-p".to_string(),
                    "--resume".to_string(),
                    resolved.operation.runtime_session_id.clone(),
                    "--output-format".to_string(),
                    "json".to_string(),
                ]
            } else {
                return Err(format!("unsupported supervised tool: {}", resolved.tool));
            };
            if let Some(model) = message.model() {
                args.extend([
                    if resolved.tool == "codex" {
                        "-m"
                    } else {
                        "--model"
                    }
                    .to_string(),
                    model.to_string(),
                ]);
            }
            if let Some(effort) = message.effort() {
                if resolved.tool == "codex" {
                    args.extend(["-c".to_string(), format!("model_reasoning_effort={effort}")]);
                } else {
                    args.extend(["--effort".to_string(), effort.to_string()]);
                }
            }
            args.extend(["--".to_string(), message.as_str().to_string()]);
            Ok((program, args, Some(resolved.canonical_cwd.clone())))
        }
    }
}

fn wake_program(tool: &str) -> Result<String, String> {
    let key = match tool {
        "claude" => "RELAY_WAKE_CMD_CLAUDE",
        "codex" => "RELAY_WAKE_CMD_CODEX",
        other => return Err(format!("unsupported supervised tool: {other}")),
    };
    Ok(std::env::var(key)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| tool.to_string()))
}

fn start_control_reader(stream: UnixStream, sender: SyncSender<Control>) -> Result<(), String> {
    spawn_named("control", "relay-supervisor-control", move || {
        let mut reader = BufReader::new(stream);
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => {
                    let _ = sender.send(Control::Disconnected);
                    break;
                }
                Ok(_) => {
                    let Ok(frame) = parse_frame(&line) else {
                        continue;
                    };
                    let Some(kind) = frame.get("kind").and_then(|value| value.get::<String>())
                    else {
                        continue;
                    };
                    let control = match kind.as_str() {
                        "input" => frame
                            .get("bytes_hex")
                            .and_then(|value| value.get::<String>())
                            .and_then(|value| decode_hex(value).ok())
                            .map(Control::Input),
                        "input_eof" => Some(Control::InputEof),
                        "cancel" => Some(Control::Cancel),
                        "resize" => frame_string(&frame, "rows")
                            .ok()
                            .and_then(|value| value.parse().ok())
                            .zip(
                                frame_string(&frame, "cols")
                                    .ok()
                                    .and_then(|value| value.parse().ok()),
                            )
                            .map(|(rows, cols)| Control::Resize(rows, cols)),
                        "signal" => frame_string(&frame, "signal")
                            .ok()
                            .and_then(|value| value.parse().ok())
                            .map(Control::Signal),
                        _ => None,
                    };
                    if let Some(control) = control {
                        if sender.send(control).is_err() {
                            break;
                        }
                    }
                }
            }
        }
    })
    .map(|_| ())
}

fn start_output_readers(
    outputs: ChildOutputs,
    sender: SyncSender<OutputChunk>,
) -> Result<usize, String> {
    let mut count = 0;
    for (stream, reader) in [("stdout", outputs.stdout), ("stderr", outputs.stderr)] {
        let Some(mut reader) = reader else { continue };
        let sender = sender.clone();
        spawn_named(stream, &format!("relay-supervisor-{stream}"), move || {
            let mut bytes = vec![0_u8; FRAME_BYTES];
            loop {
                match reader.read(&mut bytes) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => {
                        if sender
                            .send(OutputChunk::Bytes(stream, bytes[..read].to_vec()))
                            .is_err()
                        {
                            return;
                        }
                    }
                }
            }
            let _ = sender.send(OutputChunk::Done);
        })?;
        count += 1;
    }
    Ok(count)
}

fn write_child_input(input: &mut Option<ChildInput>, bytes: &[u8]) -> Result<(), String> {
    match input.as_mut() {
        Some(ChildInput::Pipe(writer)) => writer
            .write_all(bytes)
            .map_err(|error| format!("write supervised child stdin: {error}")),
        Some(ChildInput::Pty(writer)) => writer
            .write_all(bytes)
            .map_err(|error| format!("write supervised child PTY: {error}")),
        None => Err("supervised child stdin is closed".to_string()),
    }
}

fn resize_child_input(input: &mut Option<ChildInput>, rows: u16, cols: u16) -> Result<(), String> {
    if !(1..=4096).contains(&rows) || !(1..=4096).contains(&cols) {
        return Err("PTY size is outside 1..=4096".to_string());
    }
    let Some(ChildInput::Pty(master)) = input.as_ref() else {
        return Err("resize requested for non-PTY child".to_string());
    };
    set_terminal_size(master.as_raw_fd(), rows, cols)
}

fn signal_owned_child(child: &Child, signal: i32) -> Result<(), String> {
    // SAFETY: the supervisor owns an unreaped Child, so this pid cannot recycle.
    let result = unsafe { libc::kill(child.id() as i32, signal) };
    if result == 0 {
        Ok(())
    } else {
        Err(format!(
            "signal owned supervised child: {}",
            std::io::Error::last_os_error()
        ))
    }
}

fn cancel_and_reap_after_internal_failure(
    store: &LifecycleStore,
    args: &HiddenArgs,
    child: &mut Child,
    operation_version: &mut String,
    reason: &str,
) -> Result<(), String> {
    *operation_version = store.publish_disconnect_cancel(
        &args.operation_id,
        operation_version,
        &args.supervisor_instance_id,
    )?;
    let permit = store.mint_child_cancellation_permit(
        &args.operation_id,
        operation_version,
        &args.supervisor_instance_id,
        u64::from(child.id()),
    )?;
    store.authorize_child_cancellation(&permit)?;
    if child
        .try_wait()
        .map_err(|error| format!("poll child after {reason}: {error}"))?
        .is_none()
    {
        child
            .kill()
            .map_err(|error| format!("kill child after {reason}: {error}"))?;
    }
    let status = child
        .wait()
        .map_err(|error| format!("reap child after {reason}: {error}"))?;
    *operation_version = store
        .refresh_supervisor_operation_version(&args.operation_id, &args.supervisor_instance_id)?;
    store.record_child_reaped(
        &args.operation_id,
        operation_version,
        &args.supervisor_instance_id,
        normalized_exit_status(&status),
    )?;
    Ok(())
}

fn spawn_named<T: Send + 'static>(
    role: &str,
    name: &str,
    task: impl FnOnce() -> T + Send + 'static,
) -> Result<thread::JoinHandle<T>, String> {
    if std::env::var("RELAY_TEST_THREAD_SPAWN_FAIL")
        .ok()
        .is_some_and(|roles| roles.split(',').any(|candidate| candidate == role))
    {
        return Err(format!("injected {role} thread spawn failure"));
    }
    thread::Builder::new()
        .name(name.to_string())
        .spawn(task)
        .map_err(|error| format!("spawn {role} thread: {error}"))
}

fn cloexec_pipe() -> Result<(File, File), String> {
    let mut descriptors = [-1_i32; 2];
    // SAFETY: descriptors points to storage for exactly two returned fds.
    if unsafe { libc::pipe(descriptors.as_mut_ptr()) } != 0 {
        return Err(format!(
            "create lifecycle bootstrap pipe: {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: pipe returned two newly owned descriptors.
    let read = unsafe { File::from_raw_fd(descriptors[0]) };
    // SAFETY: pipe returned two newly owned descriptors.
    let write = unsafe { File::from_raw_fd(descriptors[1]) };
    set_cloexec(read.as_raw_fd(), true)?;
    set_cloexec(write.as_raw_fd(), true)?;
    Ok((read, write))
}

fn set_cloexec(fd: RawFd, enabled: bool) -> Result<(), String> {
    // SAFETY: F_GETFD only reads descriptor flags.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(format!(
            "read lifecycle bootstrap fd flags: {}",
            std::io::Error::last_os_error()
        ));
    }
    let next = if enabled {
        flags | libc::FD_CLOEXEC
    } else {
        flags & !libc::FD_CLOEXEC
    };
    // SAFETY: F_SETFD updates flags on the live descriptor.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, next) } < 0 {
        return Err(format!(
            "write lifecycle bootstrap fd flags: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

fn inherited_file(fd: RawFd) -> Result<File, String> {
    if fd < 3 {
        return Err("lifecycle bootstrap fd is outside the private range".to_string());
    }
    set_cloexec(fd, true)?;
    // SAFETY: hidden bootstrap argv transfers ownership of this one inherited fd.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn prepare_detached_child(command: &mut Command, inherited_fd: RawFd) {
    // SAFETY: only async-signal-safe libc calls execute between fork and exec.
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let flags = libc::fcntl(inherited_fd, libc::F_GETFD);
            if flags < 0 || libc::fcntl(inherited_fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

fn read_bootstrap_line(file: File, timeout: Duration) -> Result<String, String> {
    let fd = file.as_raw_fd();
    let timeout_ms = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
    let mut pollfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        // SAFETY: pollfd points to one initialized entry for the live fd.
        let ready = unsafe { libc::poll(&mut pollfd, 1, timeout_ms) };
        if ready > 0 {
            break;
        }
        if ready == 0 {
            return Err("lifecycle bootstrap pipe timed out".to_string());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(format!("poll lifecycle bootstrap pipe: {error}"));
        }
    }
    let mut line = String::new();
    BufReader::new(file)
        .read_line(&mut line)
        .map_err(|error| format!("read lifecycle bootstrap pipe: {error}"))?;
    if line.len() > 4096 || !line.ends_with('\n') {
        return Err("lifecycle bootstrap frame is incomplete or oversized".to_string());
    }
    Ok(line.trim_end().to_string())
}

fn random_hex(bytes: usize) -> Result<String, String> {
    let mut random = vec![0_u8; bytes];
    File::open("/dev/urandom")
        .and_then(|mut source| source.read_exact(&mut random))
        .map_err(|error| format!("read lifecycle control nonce: {error}"))?;
    Ok(encode_hex(&random))
}

fn open_pty(rows: u16, cols: u16) -> Result<(File, File), String> {
    let mut master = -1;
    let mut slave = -1;
    let winsize = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: output pointers are valid and initialized only on success.
    #[cfg(target_vendor = "apple")]
    let result = unsafe {
        let mut winsize = winsize;
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut winsize,
        )
    };
    // SAFETY: output pointers are valid and initialized only on success.
    #[cfg(not(target_vendor = "apple"))]
    let result = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            &winsize,
        )
    };
    if result != 0 {
        return Err(format!("open PTY: {}", std::io::Error::last_os_error()));
    }
    // SAFETY: openpty returned two newly owned descriptors.
    Ok(unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) })
}

fn terminal_size(fd: i32) -> Result<(u16, u16), String> {
    let mut winsize = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: winsize is a valid output buffer for TIOCGWINSZ.
    let result = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ as _, &mut winsize) };
    if result == 0 && winsize.ws_row > 0 && winsize.ws_col > 0 {
        Ok((winsize.ws_row, winsize.ws_col))
    } else {
        Err(format!(
            "read terminal size: {}",
            std::io::Error::last_os_error()
        ))
    }
}

fn set_terminal_size(fd: i32, rows: u16, cols: u16) -> Result<(), String> {
    let winsize = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: winsize is a valid input buffer for TIOCSWINSZ.
    let result = unsafe { libc::ioctl(fd, libc::TIOCSWINSZ as _, &winsize) };
    if result == 0 {
        Ok(())
    } else {
        Err(format!(
            "set terminal size: {}",
            std::io::Error::last_os_error()
        ))
    }
}

pub(crate) fn observe_process(pid: u32) -> ProcessObservation {
    #[cfg(target_os = "linux")]
    let start = fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| {
            let end = stat.rfind(") ")?;
            stat[end + 2..]
                .split_whitespace()
                .nth(19)
                .and_then(|value| value.parse().ok())
        })
        .map(StartGeneration::LinuxProcStartTicks)
        .unwrap_or(StartGeneration::Unavailable);
    #[cfg(not(target_os = "linux"))]
    let start = StartGeneration::Unavailable;
    // SAFETY: getpgid is observation only; failure is represented as None.
    let pgid = unsafe { libc::getpgid(pid as i32) };
    ProcessObservation {
        pid,
        pgid: (pgid >= 0).then_some(pgid),
        start,
    }
}

fn pid_exists(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| {
                let end = stat.rfind(") ")?;
                stat[end + 2..]
                    .split_whitespace()
                    .next()
                    .map(str::to_string)
            })
            .is_some_and(|state| state != "Z")
    }
    #[cfg(not(target_os = "linux"))]
    {
        // SAFETY: signal 0 performs an observation-only existence probe.
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }
}

fn normalized_exit_status(status: &ExitStatus) -> i32 {
    status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(1)
}

#[cfg(target_os = "linux")]
fn verify_same_uid(stream: &UnixStream) -> Result<(), String> {
    let credentials = rustix::net::sockopt::socket_peercred(stream)
        .map_err(|error| format!("read supervisor peer credentials: {error}"))?;
    if credentials.uid == rustix::process::getuid() {
        Ok(())
    } else {
        Err("lifecycle supervisor peer uid mismatch".to_string())
    }
}

#[cfg(not(target_os = "linux"))]
fn verify_same_uid(stream: &UnixStream) -> Result<(), String> {
    let mut uid = 0;
    let mut gid = 0;
    // SAFETY: uid/gid are valid output pointers and stream is a live Unix socket.
    let result = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
    let _ = gid;
    if result == 0 && uid == unsafe { libc::geteuid() } {
        Ok(())
    } else {
        Err("lifecycle supervisor peer uid mismatch".to_string())
    }
}

fn write_frame(stream: &mut UnixStream, frame: HashMap<String, JsonValue>) -> Result<(), String> {
    let mut encoded = JsonValue::from(frame)
        .stringify()
        .map_err(|error| format!("serialize supervisor frame: {error}"))?;
    encoded.push('\n');
    stream
        .write_all(encoded.as_bytes())
        .map_err(|error| format!("write supervisor frame: {error}"))
}

fn read_frame_unbuffered(stream: &mut UnixStream) -> Result<HashMap<String, JsonValue>, String> {
    let mut bytes = Vec::new();
    let mut byte = [0_u8; 1];
    while bytes.len() <= FRAME_BYTES * 2 + 4096 {
        stream
            .read_exact(&mut byte)
            .map_err(|error| format!("read supervisor frame: {error}"))?;
        if byte[0] == b'\n' {
            let line = String::from_utf8(bytes)
                .map_err(|_| "supervisor frame is not UTF-8".to_string())?;
            return parse_frame(&line);
        }
        bytes.push(byte[0]);
    }
    Err("supervisor frame exceeds the bounded control size".to_string())
}

fn identity_frame(kind: &str, args: &HiddenArgs) -> HashMap<String, JsonValue> {
    let mut frame = HashMap::new();
    frame.insert("kind".into(), JsonValue::from(kind.to_string()));
    frame.insert(
        "supervisor_instance_id".into(),
        JsonValue::from(args.supervisor_instance_id.clone()),
    );
    frame.insert(
        "operation_id".into(),
        JsonValue::from(args.operation_id.clone()),
    );
    frame.insert(
        "operation_version".into(),
        JsonValue::from(args.operation_version.clone()),
    );
    frame.insert(
        "control_epoch".into(),
        JsonValue::from(args.control_epoch.clone()),
    );
    frame
}

fn validate_identity_frame(
    frame: &HashMap<String, JsonValue>,
    kind: &str,
    prepared: &PreparedSupervisorLaunch,
) -> Result<(), String> {
    if frame_string(frame, "kind").as_deref() == Ok(kind)
        && frame_string(frame, "supervisor_instance_id").as_deref()
            == Ok(prepared.supervisor_instance_id.as_str())
        && frame_string(frame, "operation_id").as_deref() == Ok(prepared.operation_id.as_str())
        && frame_string(frame, "operation_version").as_deref()
            == Ok(prepared.operation_version.as_str())
        && frame_string(frame, "control_epoch").as_deref() == Ok(prepared.control_epoch.as_str())
    {
        Ok(())
    } else {
        Err(format!("supervisor {kind} identity changed"))
    }
}

fn write_health_response(
    stream: &mut UnixStream,
    supervisor_instance_id: &str,
    challenge_nonce: &str,
) -> Result<(), String> {
    let mut response = HashMap::new();
    response.insert("kind".into(), JsonValue::from("pong".to_string()));
    response.insert("role".into(), JsonValue::from("health".to_string()));
    response.insert(
        "supervisor_instance_id".into(),
        JsonValue::from(supervisor_instance_id.to_string()),
    );
    response.insert(
        "challenge_nonce".into(),
        JsonValue::from(challenge_nonce.to_string()),
    );
    write_frame(stream, response)
}

fn write_frame_locked(
    writer: &Arc<Mutex<UnixStream>>,
    frame: HashMap<String, JsonValue>,
) -> Result<(), String> {
    let mut writer = writer
        .lock()
        .map_err(|_| "supervisor socket writer poisoned".to_string())?;
    write_frame(&mut writer, frame)
}

fn send_simple(writer: &Arc<Mutex<UnixStream>>, kind: &str) -> Result<(), String> {
    let mut frame = HashMap::new();
    frame.insert("kind".into(), JsonValue::from(kind.to_string()));
    write_frame_locked(writer, frame)
}

fn parse_frame(line: &str) -> Result<HashMap<String, JsonValue>, String> {
    line.trim_end()
        .parse::<JsonValue>()
        .map_err(|error| format!("parse supervisor frame: {error}"))?
        .get::<HashMap<String, JsonValue>>()
        .cloned()
        .ok_or_else(|| "supervisor frame is not an object".to_string())
}

fn frame_string(frame: &HashMap<String, JsonValue>, key: &str) -> Result<String, String> {
    frame
        .get(key)
        .and_then(|value| value.get::<String>())
        .cloned()
        .ok_or_else(|| format!("supervisor frame is missing {key}"))
}

fn reaped_status(frame: &HashMap<String, JsonValue>) -> Result<(i32, ExitStatus), String> {
    let code = frame_string(frame, "exit_status")?
        .parse::<i32>()
        .map_err(|_| "supervisor reaped status is invalid".to_string())?;
    let raw = frame_string(frame, "wait_status_raw")?
        .parse::<i32>()
        .map_err(|_| "supervisor raw wait status is invalid".to_string())?;
    Ok((code, ExitStatus::from_raw(raw)))
}

fn encode_hex(bytes: &[u8]) -> String {
    bytes.iter().fold(
        String::with_capacity(bytes.len() * 2),
        |mut output, byte| {
            write!(output, "{byte:02x}").expect("write to String");
            output
        },
    )
}

fn decode_hex(encoded: &str) -> Result<Vec<u8>, String> {
    if encoded.len() % 2 != 0 {
        return Err("supervisor byte frame has odd hex length".to_string());
    }
    (0..encoded.len())
        .step_by(2)
        .map(|index| {
            u8::from_str_radix(&encoded[index..index + 2], 16)
                .map_err(|_| "supervisor byte frame is not hex".to_string())
        })
        .collect()
}
