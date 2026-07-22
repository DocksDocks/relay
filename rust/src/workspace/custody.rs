use crate::sha256::{constant_time_eq, hex_digest, hmac};
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::time::{Duration, Instant};
use tinyjson::JsonValue;

pub const CONTROL_FRAME_MAX: usize = 16 * 1024;
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(250);
pub const HEARTBEAT_FENCE_AFTER: Duration = Duration::from_millis(750);
pub const CONTROL_KEY_BYTES: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Sender { Guardian,
Supervisor, }

impl Sender {
    fn as_str(self) -> &'static str {
        match self {
            Self::Guardian => "guardian",
            Self::Supervisor => "supervisor",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "guardian" => Some(Self::Guardian),
            "supervisor" => Some(Self::Supervisor),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PacketKind { Bootstrap,
GuardianReady,
SupervisorReady,
WorkerPrepared,
Activate,
Activated,
Heartbeat,
HeartbeatAck,
Quiesce,
Quiesced,
Terminate,
Empty,
PrepareRelease,
ReleasePrepared,
CloseLease,
LeaseClosed,
ClosedCommitted,
Fault, }

impl PacketKind {
    pub fn as_str(self) -> &'static str { match self {
        Self::Bootstrap => "BOOTSTRAP",
        Self::GuardianReady => "GUARDIAN_READY",
        Self::SupervisorReady => "SUPERVISOR_READY",
        Self::WorkerPrepared => "WORKER_PREPARED",
        Self::Activate => "ACTIVATE",
        Self::Activated => "ACTIVATED",
        Self::Heartbeat => "HEARTBEAT",
        Self::HeartbeatAck => "HEARTBEAT_ACK",
        Self::Quiesce => "QUIESCE",
        Self::Quiesced => "QUIESCED",
        Self::Terminate => "TERMINATE",
        Self::Empty => "EMPTY",
        Self::PrepareRelease => "PREPARE_RELEASE",
        Self::ReleasePrepared => "RELEASE_PREPARED",
        Self::CloseLease => "CLOSE_LEASE",
        Self::LeaseClosed => "LEASE_CLOSED",
        Self::ClosedCommitted => "CLOSED_COMMITTED",
        Self::Fault => "FAULT",
    } }

    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "BOOTSTRAP" => Self::Bootstrap,
            "GUARDIAN_READY" => Self::GuardianReady,
            "SUPERVISOR_READY" => Self::SupervisorReady,
            "WORKER_PREPARED" => Self::WorkerPrepared,
            "ACTIVATE" => Self::Activate,
            "ACTIVATED" => Self::Activated,
            "HEARTBEAT" => Self::Heartbeat,
            "HEARTBEAT_ACK" => Self::HeartbeatAck,
            "QUIESCE" => Self::Quiesce,
            "QUIESCED" => Self::Quiesced,
            "TERMINATE" => Self::Terminate,
            "EMPTY" => Self::Empty,
            "PREPARE_RELEASE" => Self::PrepareRelease,
            "RELEASE_PREPARED" => Self::ReleasePrepared,
            "CLOSE_LEASE" => Self::CloseLease,
            "LEASE_CLOSED" => Self::LeaseClosed,
            "CLOSED_COMMITTED" => Self::ClosedCommitted,
            "FAULT" => Self::Fault,
            _ => return None,
        })
    }

    fn is_heartbeat(self) -> bool {
        matches!(self, Self::Heartbeat | Self::HeartbeatAck)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PayloadValue { String(String),
Unsigned(u64),
Bool(bool),
Null, }

pub type ControlPayload = BTreeMap<String, PayloadValue>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControlPacket { pub session_id: String,
pub generation: u64,
pub sender: Sender,
pub seq: u64,
pub kind: PacketKind,
pub payload: ControlPayload,
pub mac: [u8; 32], }

impl ControlPacket {
    fn unsigned_bytes(&self) -> Result<Vec<u8>, String> {
        validate_identity(&self.session_id, self.generation, self.seq)?;
        let mut out = String::with_capacity(256);
        write!(
            out,
            "{{\"generation\":{},\"kind\":{},\"payload\":",
            self.generation,
            quote(self.kind.as_str())
        )
        .expect("String write");
        encode_payload(&self.payload, &mut out)?;
        write!(
            out,
            ",\"sender\":{},\"seq\":{},\"session_id\":{},\"v\":1}}",
            quote(self.sender.as_str()),
            self.seq,
            quote(&self.session_id)
        )
        .expect("String write");
        Ok(out.into_bytes())
    }

    pub fn encode(&self) -> Result<Vec<u8>, String> {
        validate_identity(&self.session_id, self.generation, self.seq)?;
        let mut encoded = String::with_capacity(320);
        write!(
            encoded,
            "{{\"generation\":{},\"kind\":{},\"mac\":{},\"payload\":",
            self.generation,
            quote(self.kind.as_str()),
            quote(&hex(&self.mac))
        )
        .expect("String write");
        encode_payload(&self.payload, &mut encoded)?;
        write!(
            encoded,
            ",\"sender\":{},\"seq\":{},\"session_id\":{},\"v\":1}}",
            quote(self.sender.as_str()),
            self.seq,
            quote(&self.session_id)
        )
        .expect("String write");
        if encoded.len() > CONTROL_FRAME_MAX {
            return Err("custody control packet exceeds 16384 bytes".to_string());
        }
        Ok(encoded.into_bytes())
    }

    pub fn decode(bytes: &[u8], key: &[u8; 32]) -> Result<Self, String> { if bytes.is_empty() || bytes.len() > CONTROL_FRAME_MAX || bytes.last() == Some(&b'\n') {
        return Err("custody packet must be nonempty JCS without LF and at most 16384 bytes".to_string());
    }
    let value: JsonValue = std::str::from_utf8(bytes)
        .map_err(|_| "custody packet is not UTF-8".to_string())?
        .parse()
        .map_err(|_| "custody packet is not JSON".to_string())?;
    let object = value
        .get::<HashMap<String, JsonValue>>()
        .ok_or_else(|| "custody packet is not an object".to_string())?;
    const KEYS: [&str; 8] = ["generation", "kind", "mac", "payload", "sender", "seq", "session_id", "v"];
    if object.len() != KEYS.len() || KEYS.iter().any(|key| !object.contains_key(*key)) {
        return Err("custody packet keys are not exact".to_string());
    }
    let number = |key: &str| -> Result<u64, String> {
        let n = object
            .get(key)
            .and_then(|v| v.get::<f64>())
            .copied()
            .ok_or_else(|| format!("custody packet {key} is not a number"))?;
        if !n.is_finite() || n < 0.0 || n.fract() != 0.0 || n > 9_007_199_254_740_991.0 {
            return Err(format!("custody packet {key} is not a safe unsigned integer"));
        }
        Ok(n as u64)
    };
    if number("v")? != 1 {
        return Err("unsupported custody packet version".to_string());
    }
    let string = |key: &str| {
        object
            .get(key)
            .and_then(|v| v.get::<String>())
            .cloned()
            .ok_or_else(|| format!("custody packet {key} is not a string"))
    };
    let session_id = string("session_id")?;
    let generation = number("generation")?;
    let seq = number("seq")?;
    let sender = Sender::parse(&string("sender")?)
        .ok_or_else(|| "custody packet sender is unknown".to_string())?;
    let kind = PacketKind::parse(&string("kind")?)
        .ok_or_else(|| "custody packet kind is unknown".to_string())?;
    let payload = decode_payload(
        object
            .get("payload")
            .ok_or_else(|| "custody packet payload is missing".to_string())?,
    )?;
    let mac = decode_hex_32(&string("mac")?)?;
    let packet = Self { session_id, generation, sender, seq, kind, payload, mac };
    validate_identity(&packet.session_id, packet.generation, packet.seq)?;
    let expected = hmac(key, &packet.unsigned_bytes()?);
    if !constant_time_eq(&packet.mac, &expected) {
        return Err("custody packet MAC is invalid".to_string());
    }
    if packet.encode()? != bytes {
        return Err("custody packet is not canonical JCS".to_string());
    }
    Ok(packet) }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PeerIdentity { pub pid: libc::pid_t,
pub euid: libc::uid_t,
pub gid: libc::gid_t,
pub start_token: String, }

impl PeerIdentity {
    #[cfg(target_os = "linux")]
    pub fn current() -> Result<Self, String> { let pid = unsafe { libc::getpid() };
    Ok(Self {
        pid,
        euid: unsafe { libc::geteuid() },
        gid: unsafe { libc::getegid() },
        start_token: crate::workspace::platform::linux::process_start_token(pid)?,
    }) }
}

#[derive(Debug)]
pub struct ReceivedPacket { pub packet: ControlPacket,
pub fds: Vec<OwnedFd>, }

#[derive(Debug)]
pub struct ReceivedWorkerRoot {
    pub pid: libc::pid_t,
    pub pidfd: OwnedFd,
    pub start_token: String,
    pub cgroup_membership: String,
    pub evidence_sha256: String,
}

#[derive(Debug)]
pub struct ControlEndpoint { fd: OwnedFd,
key: [u8; 32],
session_id: String,
generation: u64,
local_sender: Sender,
expected_peer: PeerIdentity,
#[cfg(target_os = "linux")]
peer_pidfd: OwnedFd,
tx_seq: u64,
rx_seq: u64,
missed_heartbeats: u8,
pending_heartbeat_seq: Option<u64>, }

impl ControlEndpoint {
    #[cfg(target_os = "linux")]
    pub fn pair() -> Result<(OwnedFd, OwnedFd), String> { let mut fds = [-1; 2];
    let rc = unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            0,
            fds.as_mut_ptr(),
        )
    };
    if rc != 0 {
        return Err(format!("create custody seqpacket pair: {}", std::io::Error::last_os_error()));
    }
    for fd in fds {
        let enabled: libc::c_int = 1;
        let rc = unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PASSCRED,
                (&enabled as *const libc::c_int).cast(),
                std::mem::size_of_val(&enabled) as libc::socklen_t,
            )
        };
        if rc != 0 {
            unsafe { libc::close(fds[0]); libc::close(fds[1]); }
            return Err(format!("enable SO_PASSCRED: {}", std::io::Error::last_os_error()));
        }
    }
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }) }

    pub fn new(
        fd: OwnedFd,
        key: [u8; 32],
        session_id: String,
        generation: u64,
        local_sender: Sender,
        expected_peer: PeerIdentity,
    ) -> Result<Self, String> {
        validate_identity(&session_id, generation, 1)?;
        #[cfg(target_os = "linux")]
        validate_control_socket(fd.as_raw_fd())?;
        #[cfg(target_os = "linux")]
        let peer_pidfd =
            crate::workspace::platform::linux::pidfd_open(expected_peer.pid)?;
        Ok(Self {
            fd,
            key,
            session_id,
            generation,
            local_sender,
            expected_peer,
            #[cfg(target_os = "linux")]
            peer_pidfd,
            tx_seq: 0,
            rx_seq: 0,
            missed_heartbeats: 0,
            pending_heartbeat_seq: None,
        })
    }

    #[cfg(target_os = "linux")]
    pub fn send(&mut self,
    kind: PacketKind,
    payload: ControlPayload,
    fds: &[RawFd],) -> Result<u64, String> { self.tx_seq = self.tx_seq.checked_add(1).ok_or_else(|| "custody send sequence overflow".to_string())?;
    let mut packet = ControlPacket {
        session_id: self.session_id.clone(),
        generation: self.generation,
        sender: self.local_sender,
        seq: self.tx_seq,
        kind,
        payload,
        mac: [0; 32],
    };
    packet.mac = hmac(&self.key, &packet.unsigned_bytes()?);
    sendmsg_packet(self.fd.as_raw_fd(), &packet.encode()?, fds)?;
    Ok(self.tx_seq) }

    #[cfg(target_os = "linux")]
    pub fn receive(&mut self, timeout: Duration, expected_fd_count: usize) -> Result<ReceivedPacket, String> { wait_readable(self.fd.as_raw_fd(), timeout)?;
    let (bytes, credentials, fds) = recvmsg_packet(self.fd.as_raw_fd())?;
    let credentials = credentials.ok_or_else(|| "custody packet has no SCM_CREDENTIALS".to_string())?;
    if credentials.pid != self.expected_peer.pid
        || credentials.uid != self.expected_peer.euid
        || credentials.gid != self.expected_peer.gid
    {
        return Err("custody packet credentials changed".to_string());
    }
    let live = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            self.peer_pidfd.as_raw_fd(),
            0,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if live != 0 {
        return Err(format!(
            "custody packet peer pidfd is no longer live: {}",
            std::io::Error::last_os_error()
        ));
    }
    let current_start = crate::workspace::platform::linux::process_start_token(credentials.pid)?;
    if current_start != self.expected_peer.start_token {
        return Err("custody packet peer start token changed".to_string());
    }
    if fds.len() != expected_fd_count {
        return Err(format!("custody packet has {} FDs; expected {expected_fd_count}", fds.len()));
    }
    let packet = ControlPacket::decode(&bytes, &self.key)?;
    if packet.session_id != self.session_id
        || packet.generation != self.generation
        || packet.sender == self.local_sender
        || packet.seq != self.rx_seq + 1
    {
        return Err("custody packet identity or sequence is invalid".to_string());
    }
    self.rx_seq = packet.seq;
    Ok(ReceivedPacket { packet, fds }) }

    #[cfg(target_os = "linux")]
    pub fn command(&mut self,
    command: PacketKind,
    payload: ControlPayload,
    expected_ack: PacketKind,
    evidence_required: bool,) -> Result<ControlPayload, String> { if command.is_heartbeat() || expected_ack.is_heartbeat() {
        return Err("heartbeat must use heartbeat()".to_string());
    }
    while self.pending_heartbeat_seq.is_some() {
        self.heartbeat()?;
    }
    let seq = self.send(command, payload, &[])?;
    let received = self.receive(HEARTBEAT_FENCE_AFTER, 0)?;
    if received.packet.kind != expected_ack {
        return Err(format!("custody command {} received {}, expected {}", command.as_str(), received.packet.kind.as_str(), expected_ack.as_str()));
    }
    match validate_ack(&received.packet.payload, seq, evidence_required)? {
        AckStatus::Ok => Ok(received.packet.payload),
        AckStatus::Fault {
            code,
            evidence_sha256,
        } => Err(format!(
            "custody command {} received fault ACK {code} ({evidence_sha256})",
            command.as_str()
        )),
    } }

    #[cfg(target_os = "linux")]
    pub fn heartbeat(&mut self) -> Result<(), String> {
        let seq = match self.pending_heartbeat_seq {
            Some(seq) => seq,
            None => {
                let seq = self.send(
                    PacketKind::Heartbeat,
                    ControlPayload::new(),
                    &[],
                )?;
                self.pending_heartbeat_seq = Some(seq);
                seq
            }
        };
        match self.receive(HEARTBEAT_INTERVAL, 0) {
            Ok(received)
                if received.packet.kind == PacketKind::HeartbeatAck
                    && matches!(
                        validate_ack(&received.packet.payload, seq, false),
                        Ok(AckStatus::Ok)
                    ) =>
            {
                self.missed_heartbeats = 0;
                self.pending_heartbeat_seq = None;
                Ok(())
            }
            Err(error)
                if error.starts_with("custody control deadline elapsed after ") =>
            {
                self.missed_heartbeats =
                    self.missed_heartbeats.saturating_add(1);
                if self.missed_heartbeats >= 3 {
                    Err(
                        "custody heartbeat fenced after three missed ACK deadlines"
                            .to_string(),
                    )
                } else {
                    Ok(())
                }
            }
            Ok(_) => Err("custody heartbeat ACK is invalid".to_string()),
            Err(error) => Err(error),
        }
    }

    #[cfg(target_os = "linux")]
    pub fn acknowledge(&mut self,
    received: &ControlPacket,
    kind: PacketKind,
    evidence_sha256: Option<&str>,) -> Result<u64, String> { let mut payload = ControlPayload::new();
    payload.insert("ack_seq".to_string(), PayloadValue::Unsigned(received.seq));
    payload.insert("status".to_string(), PayloadValue::String("ok".to_string()));
    if let Some(evidence) = evidence_sha256 {
        validate_sha256(evidence)?;
        payload.insert("evidence_sha256".to_string(), PayloadValue::String(evidence.to_string()));
    }
    self.send(kind, payload, &[]) }

    #[cfg(target_os = "linux")]
    pub fn acknowledge_fault(
        &mut self,
        received: &ControlPacket,
        kind: PacketKind,
        code: &str,
        evidence_sha256: &str,
    ) -> Result<u64, String> {
        self.send(
            kind,
            fault_ack_payload(received.seq, code, evidence_sha256)?,
            &[],
        )
    }

    pub fn peer_identity(&self) -> &PeerIdentity { &self.expected_peer }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CustodyPhase {
    Bootstrapping,
    Ready,
    Prepared,
    Active,
    Quiesced,
    Empty,
    ReleasePrepared,
    LeaseClosed,
    ClosedCommitted,
    Faulted,
}

/// The command side of one authenticated custodian link. A coordinator uses
/// one controller per custodian so PREPARE_RELEASE and close require two
/// independent exact ACKs.
#[derive(Debug)]
pub struct CustodyController {
    endpoint: ControlEndpoint,
    phase: CustodyPhase,
}

#[cfg(target_os = "linux")]
fn decode_worker_prepared(
    packet: ReceivedPacket,
) -> Result<ReceivedWorkerRoot, String> {
    if packet.packet.kind != PacketKind::WorkerPrepared
        || packet.packet.payload.len() != 4
    {
        return Err(
            "custodian did not send exact WORKER_PREPARED".to_string(),
        );
    }
    let string = |key: &str| {
        packet.packet.payload.get(key).and_then(|value| match value {
            PayloadValue::String(value) => Some(value.clone()),
            _ => None,
        })
    };
    let evidence = string("evidence_sha256")
        .ok_or_else(|| "WORKER_PREPARED has no evidence digest".to_string())?;
    validate_sha256(&evidence)?;
    let pid = match packet.packet.payload.get("pid") {
        Some(PayloadValue::Unsigned(pid))
            if *pid > 0 && *pid <= libc::pid_t::MAX as u64 =>
        {
            *pid as libc::pid_t
        }
        _ => return Err("WORKER_PREPARED PID is invalid".to_string()),
    };
    let start_token = string("start_token")
        .ok_or_else(|| "WORKER_PREPARED start token is missing".to_string())?;
    let cgroup_membership = string("cgroup_membership").ok_or_else(|| {
        "WORKER_PREPARED cgroup membership is missing".to_string()
    })?;
    if !cgroup_membership.starts_with('/') {
        return Err(
            "WORKER_PREPARED cgroup membership is not absolute".to_string(),
        );
    }
    let expected_evidence =
        worker_prepared_evidence(pid, &start_token, &cgroup_membership)?;
    if !constant_time_eq(evidence.as_bytes(), expected_evidence.as_bytes()) {
        return Err(
            "WORKER_PREPARED evidence does not bind its identity".to_string(),
        );
    }
    let [pidfd]: [OwnedFd; 1] = packet
        .fds
        .try_into()
        .map_err(|_| "WORKER_PREPARED requires exactly one pidfd".to_string())?;
    crate::workspace::platform::linux::validate_pidfd_identity(
        pidfd.as_raw_fd(),
        pid,
        &start_token,
    )?;
    Ok(ReceivedWorkerRoot {
        pid,
        pidfd,
        start_token,
        cgroup_membership,
        evidence_sha256: evidence,
    })
}

fn custody_fault_evidence(code: &str, error: &str) -> String {
    hex_digest(format!("custody-fault-v1\0{code}\0{error}").as_bytes())
}

impl CustodyController {
    pub fn new(endpoint: ControlEndpoint) -> Self {
        Self {
            endpoint,
            phase: CustodyPhase::Bootstrapping,
        }
    }

    pub fn phase(&self) -> CustodyPhase {
        self.phase
    }

    #[cfg(target_os = "linux")]
    pub fn bootstrap(
        &mut self,
        payload: ControlPayload,
        fds: &[RawFd; 4],
    ) -> Result<ControlPayload, String> {
        self.require_phase(CustodyPhase::Bootstrapping)?;
        let seq = self.endpoint.send(PacketKind::Bootstrap, payload, fds)?;
        let ready = self
            .endpoint
            .receive(HEARTBEAT_FENCE_AFTER, 0)?;
        if !matches!(
            ready.packet.kind,
            PacketKind::GuardianReady | PacketKind::SupervisorReady
        ) {
            return Err("BOOTSTRAP did not receive a custodian READY ACK".to_string());
        }
        match validate_ack(&ready.packet.payload, seq, true)? {
            AckStatus::Ok => {}
            AckStatus::Fault {
                code,
                evidence_sha256,
            } => {
                return Err(format!(
                    "BOOTSTRAP received fault ACK {code} ({evidence_sha256})"
                ));
            }
        }
        self.phase = CustodyPhase::Ready;
        Ok(ready.packet.payload)
    }

    #[cfg(target_os = "linux")]
    pub fn worker_prepared(&mut self) -> Result<ReceivedWorkerRoot, String> {
        self.require_phase(CustodyPhase::Ready)?;
        let packet = self.endpoint.receive(HEARTBEAT_FENCE_AFTER, 1)?;
        let command = packet.packet.clone();
        let root = match decode_worker_prepared(packet) {
            Ok(root) => root,
            Err(error) => {
                let evidence =
                    custody_fault_evidence("worker_prepared_invalid", &error);
                return match self.endpoint.acknowledge_fault(
                    &command,
                    PacketKind::WorkerPrepared,
                    "worker_prepared_invalid",
                    &evidence,
                ) {
                    Ok(_) => Err(error),
                    Err(ack_error) => Err(format!(
                        "{error}; send authenticated fault ACK: {ack_error}"
                    )),
                };
            }
        };
        self.endpoint.acknowledge(
            &command,
            PacketKind::WorkerPrepared,
            Some(&root.evidence_sha256),
        )?;
        self.phase = CustodyPhase::Prepared;
        Ok(root)
    }

    #[cfg(target_os = "linux")]
    pub fn activate(&mut self) -> Result<ControlPayload, String> {
        self.require_phase(CustodyPhase::Prepared)?;
        let payload = self.endpoint.command(
            PacketKind::Activate,
            ControlPayload::new(),
            PacketKind::Activated,
            true,
        )?;
        self.phase = CustodyPhase::Active;
        Ok(payload)
    }

    #[cfg(target_os = "linux")]
    pub fn heartbeat(&mut self) -> Result<(), String> {
        if !matches!(self.phase, CustodyPhase::Active | CustodyPhase::Empty) {
            return Err("custody heartbeat is outside the live phases".to_string());
        }
        self.endpoint.heartbeat().inspect_err(|_| {
            self.phase = CustodyPhase::Faulted;
        })
    }

    #[cfg(target_os = "linux")]
    pub fn quiesce(&mut self) -> Result<ControlPayload, String> {
        self.require_phase(CustodyPhase::Active)?;
        let payload = self.endpoint.command(
            PacketKind::Quiesce,
            ControlPayload::new(),
            PacketKind::Quiesced,
            true,
        )?;
        self.phase = CustodyPhase::Quiesced;
        Ok(payload)
    }

    #[cfg(target_os = "linux")]
    pub fn confirm_empty(&mut self, evidence_sha256: &str) -> Result<(), String> {
        self.require_phase(CustodyPhase::Quiesced)?;
        validate_sha256(evidence_sha256)?;
        let empty = self.endpoint.receive(HEARTBEAT_FENCE_AFTER, 0)?;
        if empty.packet.kind != PacketKind::Empty
            || empty.packet.payload != evidence_payload(evidence_sha256)?
        {
            let error =
                "QUIESCED custody did not provide exact EMPTY evidence";
            let fault_evidence =
                custody_fault_evidence("empty_invalid", error);
            return match self.endpoint.acknowledge_fault(
                &empty.packet,
                PacketKind::Empty,
                "empty_invalid",
                &fault_evidence,
            ) {
                Ok(_) => Err(error.to_string()),
                Err(ack_error) => Err(format!(
                    "{error}; send authenticated fault ACK: {ack_error}"
                )),
            };
        }
        self.endpoint.acknowledge(
            &empty.packet,
            PacketKind::Empty,
            Some(evidence_sha256),
        )?;
        self.phase = CustodyPhase::Empty;
        Ok(())
    }

    #[cfg(target_os = "linux")]
    pub fn terminate(&mut self) -> Result<ControlPayload, String> {
        self.empty_command(PacketKind::Terminate, PacketKind::Empty)
    }

    #[cfg(target_os = "linux")]
    pub fn prepare_release(&mut self) -> Result<ControlPayload, String> {
        self.require_phase(CustodyPhase::Empty)?;
        let payload = self.endpoint.command(
            PacketKind::PrepareRelease,
            ControlPayload::new(),
            PacketKind::ReleasePrepared,
            true,
        )?;
        self.phase = CustodyPhase::ReleasePrepared;
        Ok(payload)
    }

    #[cfg(target_os = "linux")]
    pub fn close_lease(&mut self) -> Result<ControlPayload, String> {
        self.require_phase(CustodyPhase::ReleasePrepared)?;
        let payload = self.endpoint.command(
            PacketKind::CloseLease,
            ControlPayload::new(),
            PacketKind::LeaseClosed,
            true,
        )?;
        self.phase = CustodyPhase::LeaseClosed;
        Ok(payload)
    }

    #[cfg(target_os = "linux")]
    pub fn closed_committed(&mut self, evidence_sha256: &str) -> Result<(), String> {
        self.require_phase(CustodyPhase::LeaseClosed)?;
        let payload = evidence_payload(evidence_sha256)?;
        self.endpoint.command(
            PacketKind::ClosedCommitted,
            payload,
            PacketKind::ClosedCommitted,
            true,
        )?;
        self.phase = CustodyPhase::ClosedCommitted;
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn empty_command(
        &mut self,
        command: PacketKind,
        response: PacketKind,
    ) -> Result<ControlPayload, String> {
        self.require_phase(CustodyPhase::Active)?;
        let payload = self.endpoint.command(
            command,
            ControlPayload::new(),
            response,
            true,
        )?;
        self.phase = CustodyPhase::Empty;
        Ok(payload)
    }

    fn require_phase(&self, expected: CustodyPhase) -> Result<(), String> {
        if self.phase != expected {
            return Err(format!(
                "custody protocol phase is {:?}; expected {expected:?}",
                self.phase
            ));
        }
        Ok(())
    }
}

/// Receive side shared by guardian and supervisor. A 750ms command silence,
/// EOF, malformed packet, or peer identity drift is returned as a fatal fence
/// reason; HEARTBEAT is acknowledged internally.
#[derive(Debug)]
pub struct CustodianServer {
    endpoint: ControlEndpoint,
}

impl CustodianServer {
    pub fn new(endpoint: ControlEndpoint) -> Self {
        Self { endpoint }
    }

    #[cfg(target_os = "linux")]
    pub fn next_command(
        &mut self,
        expected: PacketKind,
        expected_fd_count: usize,
    ) -> Result<ReceivedPacket, String> {
        self.next_admitted(&[expected], expected_fd_count)
    }

    #[cfg(target_os = "linux")]
    pub fn next_admitted(
        &mut self,
        admitted: &[PacketKind],
        expected_fd_count: usize,
    ) -> Result<ReceivedPacket, String> {
        loop {
            let packet = self
                .endpoint
                .receive(HEARTBEAT_FENCE_AFTER, expected_fd_count)?;
            if packet.packet.kind == PacketKind::Heartbeat {
                if expected_fd_count != 0 || !packet.fds.is_empty() {
                    return Err("HEARTBEAT carried unexpected FDs".to_string());
                }
                self.endpoint.acknowledge(
                    &packet.packet,
                    PacketKind::HeartbeatAck,
                    None,
                )?;
                continue;
            }
            if packet.packet.kind == PacketKind::Fault {
                let (code, evidence) =
                    validate_fault_payload(&packet.packet.payload)?;
                self.endpoint.acknowledge(
                    &packet.packet,
                    PacketKind::Fault,
                    Some(&evidence),
                )?;
                return Err(format!(
                    "peer sent authenticated FAULT {code} ({evidence})"
                ));
            }
            if !admitted.contains(&packet.packet.kind) {
                return Err(format!(
                    "custodian received {}; kind is not admitted in this phase",
                    packet.packet.kind.as_str(),
                ));
            }
            return Ok(packet);
        }
    }

    #[cfg(target_os = "linux")]
    pub fn wait_ack(
        &mut self,
        response: PacketKind,
        command_seq: u64,
        evidence_sha256: Option<&str>,
    ) -> Result<(), String> {
        let packet = self.endpoint.receive(HEARTBEAT_FENCE_AFTER, 0)?;
        if packet.packet.kind != response {
            return Err(format!(
                "custodian ACK kind is {}; expected {}",
                packet.packet.kind.as_str(),
                response.as_str(),
            ));
        }
        match validate_ack(
            &packet.packet.payload,
            command_seq,
            evidence_sha256.is_some(),
        )? {
            AckStatus::Ok => {}
            AckStatus::Fault {
                code,
                evidence_sha256,
            } => {
                return Err(format!(
                    "custodian returned fault ACK {code} ({evidence_sha256})"
                ));
            }
        }
        if let Some(expected) = evidence_sha256 {
            if packet.packet.payload.get("evidence_sha256")
                != Some(&PayloadValue::String(expected.to_string()))
            {
                return Err("custodian ACK evidence digest changed".to_string());
            }
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    pub fn acknowledge(
        &mut self,
        command: &ControlPacket,
        response: PacketKind,
        evidence_sha256: &str,
    ) -> Result<(), String> {
        self.endpoint
            .acknowledge(command, response, Some(evidence_sha256))?;
        Ok(())
    }

    #[cfg(target_os = "linux")]
    pub fn acknowledge_fault(
        &mut self,
        command: &ControlPacket,
        response: PacketKind,
        code: &str,
        evidence_sha256: &str,
    ) -> Result<(), String> {
        self.endpoint.acknowledge_fault(
            command,
            response,
            code,
            evidence_sha256,
        )?;
        Ok(())
    }

    #[cfg(target_os = "linux")]
    pub fn send_worker_prepared(
        &mut self,
        pid: libc::pid_t,
        pidfd: RawFd,
        start_token: &str,
        cgroup_membership: &str,
        evidence_sha256: &str,
    ) -> Result<u64, String> {
        if pid <= 0 || !cgroup_membership.starts_with('/') {
            return Err("WORKER_PREPARED identity is invalid".to_string());
        }
        validate_sha256(evidence_sha256)?;
        let payload = BTreeMap::from([
            (
                "cgroup_membership".to_string(),
                PayloadValue::String(cgroup_membership.to_string()),
            ),
            (
                "evidence_sha256".to_string(),
                PayloadValue::String(evidence_sha256.to_string()),
            ),
            ("pid".to_string(), PayloadValue::Unsigned(pid as u64)),
            (
                "start_token".to_string(),
                PayloadValue::String(start_token.to_string()),
            ),
        ]);
        self.endpoint
            .send(PacketKind::WorkerPrepared, payload, &[pidfd])
    }

    #[cfg(target_os = "linux")]
    pub fn send_empty(&mut self, evidence_sha256: &str) -> Result<u64, String> {
        self.endpoint
            .send(PacketKind::Empty, evidence_payload(evidence_sha256)?, &[])
    }

    #[cfg(target_os = "linux")]
    pub fn send_fault(
        &mut self,
        code: &str,
        evidence_sha256: &str,
    ) -> Result<u64, String> {
        self.endpoint
            .send(PacketKind::Fault, fault_payload(code, evidence_sha256)?, &[])
    }
}

#[derive(Debug)]
pub struct LeaseReference {
    fd: Option<OwnedFd>,
    dev: u64,
    ino: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LeaseCloseEvidence {
    pub dev: u64,
    pub ino: u64,
    pub evidence_sha256: String,
}

pub fn set_fd_inheritable_for_exec(
    fd: RawFd,
    inheritable: bool,
) -> Result<(), String> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(format!(
            "read bootstrap FD flags: {}",
            std::io::Error::last_os_error()
        ));
    }
    let updated = if inheritable {
        flags & !libc::FD_CLOEXEC
    } else {
        flags | libc::FD_CLOEXEC
    };
    if unsafe { libc::fcntl(fd, libc::F_SETFD, updated) } != 0 {
        return Err(format!(
            "set bootstrap FD inheritance: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

impl LeaseReference {
    #[cfg(target_os = "linux")]
    pub fn from_owned_fd(fd: OwnedFd) -> Result<Self, String> {
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd.as_raw_fd(), &mut stat) } != 0 {

            return Err(format!(
                "stat custodian lease FD: {}",
                std::io::Error::last_os_error()
            ));
        }
        if stat.st_mode & libc::S_IFMT != libc::S_IFREG
            || stat.st_mode & 0o777 != 0o600
            || stat.st_uid != unsafe { libc::geteuid() }
            || stat.st_nlink != 1
        {
            return Err(
                "custodian lease FD is not an EUID-owned nlink-1 mode-0600 regular file"
                    .to_string(),
            );
        }
        Ok(Self {
            fd: Some(fd),
            dev: stat.st_dev,
            ino: stat.st_ino,
        })
    }

    pub fn as_raw_fd(&self) -> RawFd {
        self.fd
            .as_ref()
            .expect("lease reference is present until close")
            .as_raw_fd()
    }

    pub fn close(mut self) -> LeaseCloseEvidence {
        drop(self.fd.take());
        let evidence_sha256 = hex_digest(
            format!("custodian-lease-close-v1\0{}\0{}", self.dev, self.ino)
                .as_bytes(),
        );
        LeaseCloseEvidence {
            dev: self.dev,
            ino: self.ino,
            evidence_sha256,
        }
    }
}

pub fn fault_payload(code: &str, evidence_sha256: &str) -> Result<ControlPayload, String> { if code.is_empty() || !code.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_') {
    return Err("custody fault code must be nonempty lowercase ASCII".to_string());
}
validate_sha256(evidence_sha256)?;
Ok(BTreeMap::from([
    ("code".to_string(), PayloadValue::String(code.to_string())),
    ("evidence_sha256".to_string(), PayloadValue::String(evidence_sha256.to_string())),
])) }

pub fn evidence_payload(evidence_sha256: &str) -> Result<ControlPayload, String> { validate_sha256(evidence_sha256)?;
Ok(BTreeMap::from([("evidence_sha256".to_string(), PayloadValue::String(evidence_sha256.to_string()))])) }

pub fn worker_prepared_evidence(
    pid: libc::pid_t,
    start_token: &str,
    cgroup_membership: &str,
) -> Result<String, String> {
    if pid <= 0
        || start_token.is_empty()
        || !start_token.bytes().all(|byte| byte.is_ascii_digit())
        || !cgroup_membership.starts_with('/')
    {
        return Err("WORKER_PREPARED identity is invalid".to_string());
    }
    Ok(hex_digest(
        format!(
            "worker-prepared-v1\0{pid}\0{start_token}\0{cgroup_membership}\0sandbox=1"
        )
        .as_bytes(),
    ))
}

pub fn create_control_key_memfd() -> Result<(OwnedFd, [u8; 32]), String> { #[cfg(not(target_os = "linux"))]
{ return Err(crate::workspace::platform::macos::STOP_REASON.to_string()); }
#[cfg(target_os = "linux")]
{
    let mut key = [0_u8; 32];
    let got = unsafe { libc::getrandom(key.as_mut_ptr().cast(), key.len(), 0) };
    if got != key.len() as isize {
        return Err(format!("create custody control key: {}", std::io::Error::last_os_error()));
    }
    let name = b"session-relay-custody-key\0";
    let raw = unsafe { libc::memfd_create(name.as_ptr().cast(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING) };
    if raw < 0 { return Err(format!("create custody key memfd: {}", std::io::Error::last_os_error())); }
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let duplicate = unsafe { libc::dup(fd.as_raw_fd()) };
    if duplicate < 0 {
        return Err(format!(
            "duplicate custody key memfd: {}",
            std::io::Error::last_os_error()
        ));
    }
    let mut file = unsafe { File::from_raw_fd(duplicate) };
    file.write_all(&key).map_err(|e| format!("write custody key memfd: {e}"))?;
    file.sync_all().map_err(|e| format!("sync custody key memfd: {e}"))?;
    let seals = libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE;
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_ADD_SEALS, seals) } != 0 {
        return Err(format!("seal custody key memfd: {}", std::io::Error::last_os_error()));
    }
    Ok((fd, key))
} }

pub fn read_control_key_memfd(fd: RawFd) -> Result<[u8; 32], String> { #[cfg(not(target_os = "linux"))]
{ let _ = fd; return Err(crate::workspace::platform::macos::STOP_REASON.to_string()); }
#[cfg(target_os = "linux")]
{
    let expected = libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE;
    let seals = unsafe { libc::fcntl(fd, libc::F_GET_SEALS) };
    if seals < 0 || seals & expected != expected { return Err("custody key memfd is not fully sealed".to_string()); }
    let duplicate = unsafe { libc::dup(fd) };
    if duplicate < 0 { return Err(format!("duplicate custody key memfd: {}", std::io::Error::last_os_error())); }
    let mut file = unsafe { File::from_raw_fd(duplicate) };
    if file.metadata().map_err(|e| format!("stat custody key memfd: {e}"))?.len() != 32 {
        return Err("custody key memfd is not exactly 32 bytes".to_string());
    }
    file.seek(SeekFrom::Start(0)).map_err(|e| format!("seek custody key memfd: {e}"))?;
    let mut key = [0; 32];
    file.read_exact(&mut key).map_err(|e| format!("read custody key memfd: {e}"))?;
    let mut extra = [0; 1];
    if file.read(&mut extra).map_err(|e| format!("read custody key tail: {e}"))? != 0 {
        return Err("custody key memfd contains trailing bytes".to_string());
    }
    Ok(key)
} }

fn validate_identity(session_id: &str, generation: u64, seq: u64) -> Result<(), String> {
    let bytes = session_id.as_bytes();
    if bytes.len() != 36
        || bytes[8] != b'-' || bytes[13] != b'-' || bytes[14] != b'4'
        || bytes[18] != b'-' || !matches!(bytes[19], b'8' | b'9' | b'a' | b'b')
        || bytes[23] != b'-'
        || bytes.iter().enumerate().any(|(i, b)| !matches!(i, 8 | 13 | 18 | 23) && !matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    { return Err("custody session_id is not lowercase UUIDv4".to_string()); }
    if generation == 0 || seq == 0 { return Err("custody generation and sequence must be positive".to_string()); }
    Ok(())
}

fn validate_sha256(value: &str) -> Result<(), String> {
    if value.len() != 64 || !value.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return Err("custody evidence digest is not lowercase SHA-256".to_string());
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum AckStatus {
    Ok,
    Fault {
        code: String,
        evidence_sha256: String,
    },
}

fn validate_fault_code(code: &str) -> Result<(), String> {
    if code.is_empty()
        || !code.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || byte == b'_'
        })
    {
        return Err(
            "custody fault code must be nonempty lowercase ASCII".to_string(),
        );
    }
    Ok(())
}

fn validate_fault_payload(
    payload: &ControlPayload,
) -> Result<(String, String), String> {
    if payload.len() != 2 {
        return Err("custody FAULT payload is not exact".to_string());
    }
    let code = match payload.get("code") {
        Some(PayloadValue::String(code)) => code.clone(),
        _ => return Err("custody FAULT code is missing".to_string()),
    };
    validate_fault_code(&code)?;
    let evidence_sha256 = match payload.get("evidence_sha256") {
        Some(PayloadValue::String(evidence_sha256)) => {
            evidence_sha256.clone()
        }
        _ => return Err("custody FAULT evidence is missing".to_string()),
    };
    validate_sha256(&evidence_sha256)?;
    Ok((code, evidence_sha256))
}

fn fault_ack_payload(
    ack_seq: u64,
    code: &str,
    evidence_sha256: &str,
) -> Result<ControlPayload, String> {
    validate_fault_code(code)?;
    validate_sha256(evidence_sha256)?;
    Ok(BTreeMap::from([
        ("ack_seq".to_string(), PayloadValue::Unsigned(ack_seq)),
        (
            "code".to_string(),
            PayloadValue::String(code.to_string()),
        ),
        (
            "evidence_sha256".to_string(),
            PayloadValue::String(evidence_sha256.to_string()),
        ),
        (
            "status".to_string(),
            PayloadValue::String("fault".to_string()),
        ),
    ]))
}

fn validate_ack(
    payload: &ControlPayload,
    ack_seq: u64,
    evidence_required: bool,
) -> Result<AckStatus, String> {
    if payload.get("ack_seq") != Some(&PayloadValue::Unsigned(ack_seq)) {
        return Err("custody ACK is not exact".to_string());
    }
    match payload.get("status") {
        Some(PayloadValue::String(status)) if status == "ok" => {
            let allowed = if evidence_required { 3 } else { 2 };
            if payload.len() != allowed
                || (evidence_required
                    && !matches!(
                        payload.get("evidence_sha256"),
                        Some(PayloadValue::String(value))
                            if validate_sha256(value).is_ok()
                    ))
            {
                return Err("custody ACK is not exact".to_string());
            }
            Ok(AckStatus::Ok)
        }
        Some(PayloadValue::String(status)) if status == "fault" => {
            if payload.len() != 4 {
                return Err("custody fault ACK is not exact".to_string());
            }
            let code = match payload.get("code") {
                Some(PayloadValue::String(code)) => code.clone(),
                _ => return Err("custody fault ACK code is missing".to_string()),
            };
            validate_fault_code(&code)?;
            let evidence_sha256 = match payload.get("evidence_sha256") {
                Some(PayloadValue::String(evidence_sha256)) => {
                    evidence_sha256.clone()
                }
                _ => {
                    return Err(
                        "custody fault ACK evidence is missing".to_string(),
                    );
                }
            };
            validate_sha256(&evidence_sha256)?;
            Ok(AckStatus::Fault {
                code,
                evidence_sha256,
            })
        }
        _ => Err("custody ACK is not exact".to_string()),
    }
}

fn encode_payload(payload: &ControlPayload, out: &mut String) -> Result<(), String> {
    out.push('{');
    for (index, (key, value)) in payload.iter().enumerate() {
        if key.is_empty() || !key.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_') {
            return Err("custody payload key is not lowercase ASCII".to_string());
        }
        if index != 0 { out.push(','); }
        out.push_str(&quote(key));
        out.push(':');
        match value {
            PayloadValue::String(value) => out.push_str(&quote(value)),
            PayloadValue::Unsigned(value) => write!(out, "{value}").expect("String write"),
            PayloadValue::Bool(value) => out.push_str(if *value { "true" } else { "false" }),
            PayloadValue::Null => out.push_str("null"),
        }
    }
    out.push('}');
    Ok(())
}

fn decode_payload(value: &JsonValue) -> Result<ControlPayload, String> {
    let object = value.get::<HashMap<String, JsonValue>>().ok_or_else(|| "custody payload is not an object".to_string())?;
    let mut payload = ControlPayload::new();
    for (key, value) in object {
        let decoded = if let Some(value) = value.get::<String>() {
            PayloadValue::String(value.clone())
        } else if let Some(value) = value.get::<bool>() {
            PayloadValue::Bool(*value)
        } else if value.get::<()>().is_some() {
            PayloadValue::Null
        } else if let Some(value) = value.get::<f64>() {
            if !value.is_finite() || *value < 0.0 || value.fract() != 0.0 || *value > 9_007_199_254_740_991.0 {
                return Err("custody payload number is not a safe unsigned integer".to_string());
            }
            PayloadValue::Unsigned(*value as u64)
        } else { return Err("custody payload contains a nested or unsupported value".to_string()); };
        payload.insert(key.clone(), decoded);
    }
    Ok(payload)
}

fn quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""), '\\' => out.push_str("\\\\"), '\u{08}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"), '\n' => out.push_str("\\n"), '\u{0c}' => out.push_str("\\f"), '\r' => out.push_str("\\r"),
            ch if ch <= '\u{1f}' => write!(out, "\\u{:04x}", ch as u32).expect("String write"),
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}


fn hex(bytes: &[u8]) -> String { bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut out, b| { write!(out, "{b:02x}").expect("String write"); out }) }
fn decode_hex_32(value: &str) -> Result<[u8; 32], String> {
    if value.len() != 64 { return Err("custody MAC is not 32-byte lowercase hex".to_string()); }
    let mut out = [0; 32];
    for (index, chunk) in value.as_bytes().chunks_exact(2).enumerate() {
        let digit = |b| match b { b'0'..=b'9' => Some(b - b'0'), b'a'..=b'f' => Some(b - b'a' + 10), _ => None };
        out[index] = digit(chunk[0]).zip(digit(chunk[1])).map(|(hi, lo)| hi << 4 | lo).ok_or_else(|| "custody MAC is not lowercase hex".to_string())?;
    }
    Ok(out)
}

#[cfg(target_os = "linux")]
fn validate_control_socket(fd: RawFd) -> Result<(), String> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0
        || unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) }
            != 0
    {
        return Err(format!(
            "set custody socket CLOEXEC: {}",
            std::io::Error::last_os_error()
        ));
    }
    let mut socket_type = 0_i32;
    let mut type_len = std::mem::size_of::<i32>() as libc::socklen_t;
    let type_rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            (&mut socket_type as *mut i32).cast(),
            &mut type_len,
        )
    };
    let mut domain = 0_i32;
    let mut domain_len = std::mem::size_of::<i32>() as libc::socklen_t;
    let domain_rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_DOMAIN,
            (&mut domain as *mut i32).cast(),
            &mut domain_len,
        )
    };
    if type_rc != 0
        || domain_rc != 0
        || socket_type != libc::SOCK_SEQPACKET
        || domain != libc::AF_UNIX
    {
        return Err("custody control FD is not an AF_UNIX SOCK_SEQPACKET".to_string());
    }
    let enabled: libc::c_int = 1;
    if unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PASSCRED,
            (&enabled as *const libc::c_int).cast(),
            std::mem::size_of_val(&enabled) as libc::socklen_t,
        )
    } != 0
    {
        return Err(format!(
            "enable custody SO_PASSCRED: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn sendmsg_packet(fd: RawFd, bytes: &[u8], rights: &[RawFd]) -> Result<(), String> {
    if rights.len() > 4 { return Err("custody packet carries more than four FDs".to_string()); }
    let mut iov = libc::iovec { iov_base: bytes.as_ptr().cast_mut().cast(), iov_len: bytes.len() };
    let control_len = if rights.is_empty() { 0 } else { unsafe { libc::CMSG_SPACE(std::mem::size_of_val(rights) as u32) as usize } };
    let words = control_len.div_ceil(std::mem::size_of::<usize>());
    let mut control = vec![0_usize; words];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov; msg.msg_iovlen = 1;
    if !rights.is_empty() {
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = control_len;
        let cmsg = unsafe { libc::CMSG_FIRSTHDR(&msg) };
        unsafe {
            (*cmsg).cmsg_level = libc::SOL_SOCKET; (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of_val(rights) as u32) as usize;
            std::ptr::copy_nonoverlapping(rights.as_ptr(), libc::CMSG_DATA(cmsg).cast(), rights.len());
        }
    }
    let sent = unsafe { libc::sendmsg(fd, &msg, libc::MSG_NOSIGNAL) };
    if sent != bytes.len() as isize { return Err(format!("send custody packet: {}", std::io::Error::last_os_error())); }
    Ok(())
}

#[cfg(target_os = "linux")]
fn recvmsg_packet(fd: RawFd) -> Result<(Vec<u8>, Option<libc::ucred>, Vec<OwnedFd>), String> {
    let mut bytes = vec![0_u8; CONTROL_FRAME_MAX];
    let mut iov = libc::iovec { iov_base: bytes.as_mut_ptr().cast(), iov_len: bytes.len() };
    let control_len = unsafe { libc::CMSG_SPACE(std::mem::size_of::<libc::ucred>() as u32) + libc::CMSG_SPACE((4 * std::mem::size_of::<RawFd>()) as u32) } as usize;
    let words = control_len.div_ceil(std::mem::size_of::<usize>());
    let mut control = vec![0_usize; words];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = control_len;
    let count = unsafe { libc::recvmsg(fd, &mut msg, libc::MSG_CMSG_CLOEXEC) };
    if count == 0 { return Err("custody control EOF".to_string()); }
    if count < 0 { return Err(format!("receive custody packet: {}", std::io::Error::last_os_error())); }
    let truncated = msg.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0;
    bytes.truncate((count as usize).min(bytes.len()));
    let mut credentials = None;
    let mut rights = Vec::new();
    let mut saw_rights = false;
    let mut duplicate_rights = false;
    let mut cmsg = unsafe { libc::CMSG_FIRSTHDR(&msg) };
    while !cmsg.is_null() {
        unsafe {
            if (*cmsg).cmsg_level != libc::SOL_SOCKET { return Err("custody packet has non-socket ancillary data".to_string()); }
            if (*cmsg).cmsg_type == libc::SCM_CREDENTIALS {
                if credentials.is_some() || (*cmsg).cmsg_len != libc::CMSG_LEN(std::mem::size_of::<libc::ucred>() as u32) as usize { return Err("custody packet SCM_CREDENTIALS is duplicate or malformed".to_string()); }
                credentials = Some(*libc::CMSG_DATA(cmsg).cast::<libc::ucred>());
            } else if (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                duplicate_rights |= saw_rights;
                saw_rights = true;
                let data_len = (*cmsg).cmsg_len.checked_sub(libc::CMSG_LEN(0) as usize).ok_or_else(|| "custody SCM_RIGHTS length underflow".to_string())?;
                if data_len % std::mem::size_of::<RawFd>() != 0 { return Err("custody SCM_RIGHTS length is malformed".to_string()); }
                let count = data_len / std::mem::size_of::<RawFd>();
                if count > 4 { return Err("custody packet has too many SCM_RIGHTS FDs".to_string()); }
                let data = libc::CMSG_DATA(cmsg).cast::<RawFd>();
                for index in 0..count { rights.push(OwnedFd::from_raw_fd(*data.add(index))); }
            } else { return Err("custody packet has unknown ancillary data".to_string()); }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
    }
    if truncated {
        return Err("custody packet or ancillary data was truncated".to_string());
    }
    if duplicate_rights {
        return Err("custody packet has duplicate SCM_RIGHTS".to_string());
    }
    if rights.len() > 4 {
        return Err("custody packet has too many SCM_RIGHTS FDs".to_string());
    }
    Ok((bytes, credentials, rights))
}

#[cfg(target_os = "linux")]
fn wait_readable(fd: RawFd, timeout: Duration) -> Result<(), String> {
    let millis = timeout.as_millis().min(i32::MAX as u128) as i32;
    let mut poll = libc::pollfd { fd, events: libc::POLLIN | libc::POLLHUP | libc::POLLERR, revents: 0 };
    let started = Instant::now();
    loop {
        let rc = unsafe { libc::poll(&mut poll, 1, millis) };
        if rc > 0 { return Ok(()); }
        if rc == 0 { return Err(format!("custody control deadline elapsed after {} ms", started.elapsed().as_millis())); }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted { return Err(format!("poll custody control: {error}")); }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    fn current_peer() -> PeerIdentity {
        let pid = unsafe { libc::getpid() };
        PeerIdentity {
            pid,
            euid: unsafe { libc::geteuid() },
            gid: unsafe { libc::getegid() },
            start_token: crate::workspace::platform::linux::process_start_token(pid)
                .unwrap(),
        }
    }

    #[test]
    fn heartbeat_fences_only_after_three_missed_deadlines() {
        let (controller_fd, _silent_peer) = ControlEndpoint::pair().unwrap();
        let endpoint = ControlEndpoint::new(
            controller_fd,
            [0x42; 32],
            "00000000-0000-4000-8000-000000000001".to_string(),
            1,
            Sender::Guardian,
            current_peer(),
        )
        .unwrap();
        let mut controller = CustodyController {
            endpoint,
            phase: CustodyPhase::Active,
        };

        assert!(controller.heartbeat().is_ok());
        assert_eq!(controller.phase(), CustodyPhase::Active);
        assert!(controller.heartbeat().is_ok());
        assert_eq!(controller.phase(), CustodyPhase::Active);
        assert!(controller.heartbeat().is_err());
        assert_eq!(controller.phase(), CustodyPhase::Faulted);
    }

    #[test]
    fn late_heartbeat_ack_satisfies_the_outstanding_deadline() {
        let peer = current_peer();
        let (guardian_fd, supervisor_fd) = ControlEndpoint::pair().unwrap();
        let key = [0x29; 32];
        let session_id =
            "00000000-0000-4000-8000-000000000001".to_string();
        let guardian_endpoint = ControlEndpoint::new(
            guardian_fd,
            key,
            session_id.clone(),
            1,
            Sender::Guardian,
            peer.clone(),
        )
        .unwrap();
        let mut supervisor_endpoint = ControlEndpoint::new(
            supervisor_fd,
            key,
            session_id,
            1,
            Sender::Supervisor,
            peer,
        )
        .unwrap();
        let responder = std::thread::spawn(move || {
            let heartbeat = supervisor_endpoint
                .receive(Duration::from_secs(1), 0)
                .unwrap();
            assert_eq!(heartbeat.packet.kind, PacketKind::Heartbeat);
            std::thread::sleep(Duration::from_millis(300));
            supervisor_endpoint
                .acknowledge(
                    &heartbeat.packet,
                    PacketKind::HeartbeatAck,
                    None,
                )
                .unwrap();
        });
        let mut controller = CustodyController {
            endpoint: guardian_endpoint,
            phase: CustodyPhase::Active,
        };

        assert!(controller.heartbeat().is_ok());
        assert!(controller.heartbeat().is_ok());
        assert_eq!(controller.phase(), CustodyPhase::Active);
        responder.join().unwrap();
    }

    #[test]
    fn lifecycle_command_drains_outstanding_heartbeat_ack() {
        let peer = current_peer();
        let (guardian_fd, supervisor_fd) = ControlEndpoint::pair().unwrap();
        let key = [0x39; 32];
        let session_id =
            "00000000-0000-4000-8000-000000000001".to_string();
        let guardian_endpoint = ControlEndpoint::new(
            guardian_fd,
            key,
            session_id.clone(),
            1,
            Sender::Guardian,
            peer.clone(),
        )
        .unwrap();
        let mut supervisor_endpoint = ControlEndpoint::new(
            supervisor_fd,
            key,
            session_id,
            1,
            Sender::Supervisor,
            peer,
        )
        .unwrap();
        let evidence = "b".repeat(64);
        let response_evidence = evidence.clone();
        let responder = std::thread::spawn(move || {
            let heartbeat = supervisor_endpoint
                .receive(Duration::from_secs(1), 0)
                .unwrap();
            assert_eq!(heartbeat.packet.kind, PacketKind::Heartbeat);
            std::thread::sleep(Duration::from_millis(300));
            supervisor_endpoint
                .acknowledge(
                    &heartbeat.packet,
                    PacketKind::HeartbeatAck,
                    None,
                )
                .unwrap();
            let quiesce = supervisor_endpoint
                .receive(Duration::from_secs(1), 0)
                .unwrap();
            assert_eq!(quiesce.packet.kind, PacketKind::Quiesce);
            supervisor_endpoint
                .acknowledge(
                    &quiesce.packet,
                    PacketKind::Quiesced,
                    Some(&response_evidence),
                )
                .unwrap();
        });
        let mut controller = CustodyController {
            endpoint: guardian_endpoint,
            phase: CustodyPhase::Active,
        };

        assert!(controller.heartbeat().is_ok());
        let response = controller.quiesce().unwrap();
        assert_eq!(
            response.get("evidence_sha256"),
            Some(&PayloadValue::String(evidence))
        );
        assert_eq!(controller.phase(), CustodyPhase::Quiesced);
        responder.join().unwrap();
    }

    #[test]
    fn worker_prepared_uses_one_shared_preimage() {
        let peer = current_peer();
        let pid = peer.pid;
        let start_token = peer.start_token.clone();
        let membership = "/session";
        let evidence =
            worker_prepared_evidence(pid, &start_token, membership).unwrap();
        let (guardian_fd, supervisor_fd) = ControlEndpoint::pair().unwrap();
        let key = [0x63; 32];
        let session_id =
            "00000000-0000-4000-8000-000000000001".to_string();
        let guardian_endpoint = ControlEndpoint::new(
            guardian_fd,
            key,
            session_id.clone(),
            1,
            Sender::Guardian,
            peer.clone(),
        )
        .unwrap();
        let supervisor_endpoint = ControlEndpoint::new(
            supervisor_fd,
            key,
            session_id,
            1,
            Sender::Supervisor,
            peer,
        )
        .unwrap();
        let expected_evidence = evidence.clone();
        let sender = std::thread::spawn(move || {
            let pidfd =
                crate::workspace::platform::linux::pidfd_open(pid).unwrap();
            let mut server = CustodianServer::new(supervisor_endpoint);
            let seq = server
                .send_worker_prepared(
                    pid,
                    pidfd.as_raw_fd(),
                    &start_token,
                    membership,
                    &expected_evidence,
                )
                .unwrap();
            server
                .wait_ack(
                    PacketKind::WorkerPrepared,
                    seq,
                    Some(&expected_evidence),
                )
                .unwrap();
        });
        let mut controller = CustodyController {
            endpoint: guardian_endpoint,
            phase: CustodyPhase::Ready,
        };
        let root = controller.worker_prepared().unwrap();
        assert_eq!(root.evidence_sha256, evidence);
        sender.join().unwrap();
    }

    #[test]
    fn fault_ack_is_exact_and_authenticated_by_packet_mac() {
        let evidence = "a".repeat(64);
        let payload =
            fault_ack_payload(7, "activation_failed", &evidence).unwrap();
        let key = [0x5a; 32];
        let mut packet = ControlPacket {
            session_id: "00000000-0000-4000-8000-000000000001".to_string(),
            generation: 1,
            sender: Sender::Supervisor,
            seq: 1,
            kind: PacketKind::Activated,
            payload,
            mac: [0; 32],
        };
        packet.mac = hmac(&key, &packet.unsigned_bytes().unwrap());
        let decoded = ControlPacket::decode(
            &packet.encode().unwrap(),
            &key,
        )
        .unwrap();
        assert_eq!(
            validate_ack(&decoded.payload, 7, true).unwrap(),
            AckStatus::Fault {
                code: "activation_failed".to_string(),
                evidence_sha256: evidence,
            }
        );
    }

    #[test]
    fn bootstrap_fault_ack_is_rejected_without_phase_progress() {
        let peer = current_peer();
        let (guardian_fd, supervisor_fd) = ControlEndpoint::pair().unwrap();
        let key = [0x71; 32];
        let session_id =
            "00000000-0000-4000-8000-000000000001".to_string();
        let guardian_endpoint = ControlEndpoint::new(
            guardian_fd,
            key,
            session_id.clone(),
            1,
            Sender::Guardian,
            peer.clone(),
        )
        .unwrap();
        let mut supervisor_endpoint = ControlEndpoint::new(
            supervisor_fd,
            key,
            session_id,
            1,
            Sender::Supervisor,
            peer,
        )
        .unwrap();
        let responder = std::thread::spawn(move || {
            let command = supervisor_endpoint
                .receive(Duration::from_secs(1), 4)
                .unwrap();
            let evidence =
                custody_fault_evidence("bootstrap_invalid", "bad bootstrap");
            supervisor_endpoint
                .acknowledge_fault(
                    &command.packet,
                    PacketKind::SupervisorReady,
                    "bootstrap_invalid",
                    &evidence,
                )
                .unwrap();
        });
        let files = [
            File::open("/dev/null").unwrap(),
            File::open("/dev/null").unwrap(),
            File::open("/dev/null").unwrap(),
            File::open("/dev/null").unwrap(),
        ];
        let fds = files.each_ref().map(AsRawFd::as_raw_fd);
        let mut controller = CustodyController::new(guardian_endpoint);
        let error = controller
            .bootstrap(ControlPayload::new(), &fds)
            .unwrap_err();
        assert!(error.contains("BOOTSTRAP received fault ACK"));
        assert_eq!(controller.phase(), CustodyPhase::Bootstrapping);
        responder.join().unwrap();
    }
}
