// appserver.rs — minimal Codex app-server client shared by relay delivery paths.
//
// Protocol spike verified 2026-07-02 on codex-cli 0.142.5:
//   - every app-server socket listener (unix:// included) speaks WebSocket —
//     HTTP Upgrade + RFC6455 frames; raw JSONL exists only on stdio. Hence the
//     hand-rolled WS client below (zero-crate budget: /dev/urandom supplies the
//     key + masks; the Sec-WebSocket-Accept SHA1 check is intentionally
//     skipped — local socket, the 101 status line is the gate).
//   - `thread/resume` accepts a raw rollout/session uuid; `thread/inject_items`
//     persists durably and is model-visible; `turn/start` with approvalPolicy
//     "never" completes unattended. The `jsonrpc` field is omitted on the wire.
//   - a `turn/start` issued immediately after inject_items wedges the turn:
//     wait RELAY_TURN_SETTLE_MS (default 5000) between the two.
//   - `approvalPolicy: "never"` auto-rejects shell approvals, but an MCP tool
//     call raises an `mcpServer/elicitation/request` server->client REQUEST
//     that MUST be answered (`{action: "accept"|"decline"}`) or the turn wedges
//     on `waitingOnApproval` forever. After `turn/start`, the client stays
//     attached until `turn/completed`, accepting the relay's own `bus` server
//     only when the registry proves the relay spawned the thread; joined or
//     foreign threads decline every elicitation.

use crate::lifecycle::{OperationKind, ReentryGuard, ServiceTier};
use crate::sha256::hex_digest;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};
use tinyjson::JsonValue;

const DEFAULT_TURN_WAIT_MS: u64 = 300_000;
const RPC_TIMEOUT_SECS: u64 = 20;
pub(crate) const ACK_NUDGE: &str = "New session-relay mail was added to this thread's context. Acknowledge receipt and respond appropriately.";

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ThreadState {
    Idle,
    Active,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DeliveryOutcome {
    Delivered,
    AckDeferred,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DeliveryError {
    BeforeInject(String),
    AfterInject(String),
}

enum GuardedRequestError {
    BeforeSend(String),
    AfterSend(String),
}

impl GuardedRequestError {
    fn into_message(self) -> String {
        match self {
            Self::BeforeSend(message) | Self::AfterSend(message) => message,
        }
    }
}

pub(crate) struct SpawnedThread {
    ws: WsConn,
    id: String,
    turn_id: Option<String>,
    server: String,
    server_fingerprint: String,
}

impl SpawnedThread {
    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn turn_id(&self) -> Option<&str> {
        self.turn_id.as_deref()
    }

    pub(crate) fn start_initial_turn_with_guard(
        &mut self,
        guard: &mut ReentryGuard,
        prompt: &str,
        model: Option<&str>,
        effort: Option<&str>,
        service_tier: ServiceTier,
    ) -> Result<(), String> {
        let target = guard.authorize_use(OperationKind::InitialTurn)?;
        if target.thread_id.as_deref() != Some(self.id.as_str())
            || target.server_fingerprint.as_deref() != Some(self.server_fingerprint.as_str())
        {
            return Err("initial-turn guard does not match spawned thread authority".to_string());
        }
        let result = self
            .ws
            .request_with_guard(
                2,
                "turn/start",
                initial_turn_params(&self.id, prompt, model, effort, service_tier),
                guard,
                OperationKind::InitialTurn,
            )
            .map_err(GuardedRequestError::into_message)?;
        let turn_id = turn_id_from_start_result(&result)?;
        self.turn_id = Some(turn_id);
        Ok(())
    }

    pub(crate) fn pump_with_guard(
        &mut self,
        guard: &mut ReentryGuard,
        timeout_ms: u64,
    ) -> Result<bool, String> {
        guard.authorize_use(OperationKind::InitialTurn)?;
        let turn_id = self
            .turn_id
            .as_deref()
            .ok_or_else(|| "initial turn identity was not recorded".to_string())?;
        self.ws
            .pump_turn_with_guard(timeout_ms, true, guard, &self.id, turn_id)
    }

    pub(crate) fn interrupt_initial_turn(&mut self, timeout_ms: u64) -> Result<bool, String> {
        let turn_id = self
            .turn_id
            .as_deref()
            .ok_or_else(|| "initial turn identity was not recorded".to_string())?;
        if self
            .ws
            .interrupt_and_confirm(3, &self.id, turn_id, timeout_ms)?
        {
            return Ok(true);
        }
        Ok(thread_state(&self.server, &self.id)? == ThreadState::Idle)
    }
}

fn die(msg: &str) -> ! {
    eprintln!("{msg}");
    std::process::exit(1);
}

pub(crate) fn deliver_with_guard(
    guard: &mut ReentryGuard,
    block: &str,
    auto_turn: bool,
    settle_ms: u64,
    allow_bus: bool,
    service_tier: ServiceTier,
) -> Result<DeliveryOutcome, DeliveryError> {
    let kind = guard.allowed();
    if !matches!(
        kind,
        OperationKind::WatchInject | OperationKind::WatchAutoTurn | OperationKind::WakeAppServer
    ) || (kind == OperationKind::WatchInject && auto_turn)
        || (kind == OperationKind::WatchAutoTurn && !auto_turn)
        || (kind == OperationKind::WakeAppServer && !auto_turn)
    {
        return Err(DeliveryError::BeforeInject(format!(
            "{} cannot authorize this delivery mode",
            kind.as_str()
        )));
    }
    let target = guard
        .authorize_use(kind)
        .map_err(DeliveryError::BeforeInject)?;
    let server = target
        .server
        .ok_or_else(|| DeliveryError::BeforeInject("guard has no app-server".to_string()))?;
    let thread_id = target
        .thread_id
        .ok_or_else(|| DeliveryError::BeforeInject("guard has no app-server thread".to_string()))?;
    let mut ws = connect_initialized_with_guard(
        &server,
        "session-relay",
        "session-relay delivery",
        guard,
        kind,
    )
    .map_err(DeliveryError::BeforeInject)?;
    let resumed = ws
        .request_with_guard(
            1,
            "thread/resume",
            resume_params(&thread_id, service_tier),
            guard,
            kind,
        )
        .map_err(|error| DeliveryError::BeforeInject(error.into_message()))?;
    verify_effective_service_tier(&resumed, service_tier, "thread/resume")
        .map_err(DeliveryError::BeforeInject)?;
    ws.request_with_guard(
        2,
        "thread/inject_items",
        inject_params(&thread_id, block),
        guard,
        kind,
    )
    .map_err(|error| match error {
        GuardedRequestError::BeforeSend(message) => DeliveryError::BeforeInject(message),
        GuardedRequestError::AfterSend(message) => DeliveryError::AfterInject(message),
    })?;
    if auto_turn {
        let settle_deadline = Instant::now() + Duration::from_millis(settle_ms);
        while Instant::now() < settle_deadline {
            if guard.cancelled() {
                return Err(DeliveryError::AfterInject(
                    "delivery cancelled during settle".to_string(),
                ));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        guard
            .authorize_use(kind)
            .map_err(DeliveryError::AfterInject)?;
        match read_status_with_guard(&mut ws, 3, &thread_id, guard, kind)
            .map_err(DeliveryError::AfterInject)?
        {
            ThreadState::Active => return Ok(DeliveryOutcome::AckDeferred),
            ThreadState::Idle => {
                start_ack_turn_with_guard(
                    &mut ws,
                    4,
                    &thread_id,
                    allow_bus,
                    service_tier,
                    guard,
                    kind,
                )
                .map_err(DeliveryError::AfterInject)?;
            }
        }
    }
    Ok(DeliveryOutcome::Delivered)
}

pub(crate) fn probe(server: &str) -> Result<(), String> {
    connect_initialized(server, "session-relay-doctor", "session-relay doctor").map(|_| ())
}

pub(crate) fn start_thread(
    server: &str,
    cwd: &str,
    model: Option<&str>,
    sandbox: &str,
    service_tier: ServiceTier,
) -> Result<SpawnedThread, String> {
    let mut ws = connect_initialized(server, "session-relay-spawn", "session-relay spawn")?;
    let result = ws.request(
        1,
        "thread/start",
        thread_start_params(cwd, model, sandbox, service_tier),
    )?;
    verify_effective_service_tier(&result, service_tier, "thread/start")?;
    let id = result
        .get::<HashMap<String, JsonValue>>()
        .and_then(|result| result.get("thread"))
        .and_then(|thread| thread.get::<HashMap<String, JsonValue>>())
        .and_then(|thread| thread.get("id"))
        .and_then(|id| id.get::<String>())
        .cloned()
        .ok_or_else(|| "thread/start response missing thread.id".to_string())?;
    Ok(SpawnedThread {
        ws,
        id,
        turn_id: None,
        server: server.to_string(),
        server_fingerprint: hex_digest(server.as_bytes()),
    })
}

fn turn_event_identity(object: &HashMap<String, JsonValue>) -> Option<(String, String)> {
    let params = object.get("params")?.get::<HashMap<String, JsonValue>>()?;
    let thread_id = params.get("threadId")?.get::<String>()?.clone();
    let turn_id = params
        .get("turn")?
        .get::<HashMap<String, JsonValue>>()?
        .get("id")?
        .get::<String>()?
        .clone();
    (!thread_id.is_empty() && !turn_id.is_empty()).then_some((thread_id, turn_id))
}

fn turn_id_from_start_result(result: &JsonValue) -> Result<String, String> {
    result
        .get::<HashMap<String, JsonValue>>()
        .and_then(|result| result.get("turn"))
        .and_then(|turn| turn.get::<HashMap<String, JsonValue>>())
        .and_then(|turn| turn.get("id"))
        .and_then(|id| id.get::<String>())
        .filter(|id| !id.is_empty())
        .cloned()
        .ok_or_else(|| "turn/start response missing turn.id".to_string())
}

pub(crate) fn thread_state(server: &str, thread_id: &str) -> Result<ThreadState, String> {
    let mut ws = connect_initialized(server, "session-relay", "session-relay status")?;
    read_status(&mut ws, 1, thread_id)
}

pub(crate) fn acknowledge_with_guard(
    guard: &mut ReentryGuard,
    allow_bus: bool,
    service_tier: ServiceTier,
) -> Result<DeliveryOutcome, String> {
    let target = guard.authorize_use(OperationKind::WatchAck)?;
    let server = target
        .server
        .ok_or_else(|| "guard has no app-server".to_string())?;
    let thread_id = target
        .thread_id
        .ok_or_else(|| "guard has no app-server thread".to_string())?;
    let mut ws = connect_initialized_with_guard(
        &server,
        "session-relay",
        "session-relay acknowledgement",
        guard,
        OperationKind::WatchAck,
    )?;
    let resumed = ws
        .request_with_guard(
            1,
            "thread/resume",
            resume_params(&thread_id, service_tier),
            guard,
            OperationKind::WatchAck,
        )
        .map_err(GuardedRequestError::into_message)?;
    verify_effective_service_tier(&resumed, service_tier, "thread/resume")?;
    if read_status_with_guard(&mut ws, 2, &thread_id, guard, OperationKind::WatchAck)?
        == ThreadState::Active
    {
        return Ok(DeliveryOutcome::AckDeferred);
    }
    start_ack_turn_with_guard(
        &mut ws,
        3,
        &thread_id,
        allow_bus,
        service_tier,
        guard,
        OperationKind::WatchAck,
    )?;
    Ok(DeliveryOutcome::Delivered)
}

fn connect_initialized(server: &str, name: &str, title: &str) -> Result<WsConn, String> {
    let mut ws = WsConn::connect(server)?;
    ws.request(
        0,
        "initialize",
        sobj(vec![(
            "clientInfo",
            sobj(vec![
                ("name", JsonValue::from(name.to_string())),
                ("title", JsonValue::from(title.to_string())),
                (
                    "version",
                    JsonValue::from(env!("CARGO_PKG_VERSION").to_string()),
                ),
            ]),
        )]),
    )?;
    ws.notify("initialized", JsonValue::from(HashMap::new()))?;
    Ok(ws)
}

fn connect_initialized_with_guard(
    server: &str,
    name: &str,
    title: &str,
    guard: &mut ReentryGuard,
    kind: OperationKind,
) -> Result<WsConn, String> {
    let mut ws = WsConn::connect_with_guard(server, guard, kind)?;
    ws.request_with_guard(
        0,
        "initialize",
        sobj(vec![(
            "clientInfo",
            sobj(vec![
                ("name", JsonValue::from(name.to_string())),
                ("title", JsonValue::from(title.to_string())),
                (
                    "version",
                    JsonValue::from(env!("CARGO_PKG_VERSION").to_string()),
                ),
            ]),
        )]),
        guard,
        kind,
    )
    .map_err(GuardedRequestError::into_message)?;
    ws.notify_with_guard("initialized", JsonValue::from(HashMap::new()), guard, kind)?;
    Ok(ws)
}

// The relay's own bus server is store-local (no shell or file tools), but it is
// accepted only for relay-spawned threads. Joined/foreign threads decline all
// elicitations so the connected client cannot grant authority it does not own.
fn elicitation_action(server_name: &str, allow_bus: bool) -> &'static str {
    if allow_bus && server_name == "bus" {
        "accept"
    } else {
        "decline"
    }
}

fn read_status(ws: &mut WsConn, id: u64, thread_id: &str) -> Result<ThreadState, String> {
    let result = ws.request(
        id,
        "thread/read",
        sobj(vec![("threadId", JsonValue::from(thread_id.to_string()))]),
    )?;
    let status_type = result
        .get::<HashMap<String, JsonValue>>()
        .and_then(|result| result.get("thread"))
        .and_then(|thread| thread.get::<HashMap<String, JsonValue>>())
        .and_then(|thread| thread.get("status"))
        .and_then(|status| status.get::<HashMap<String, JsonValue>>())
        .and_then(|status| status.get("type"))
        .and_then(|kind| kind.get::<String>())
        .map(String::as_str)
        .ok_or_else(|| "thread/read response missing thread.status.type".to_string())?;
    match status_type {
        "idle" => Ok(ThreadState::Idle),
        "active" => Ok(ThreadState::Active),
        other => Err(format!(
            "thread is not ready for relay delivery (status {other})"
        )),
    }
}

fn read_status_with_guard(
    ws: &mut WsConn,
    id: u64,
    thread_id: &str,
    guard: &mut ReentryGuard,
    kind: OperationKind,
) -> Result<ThreadState, String> {
    let result = ws
        .request_with_guard(
            id,
            "thread/read",
            sobj(vec![("threadId", JsonValue::from(thread_id.to_string()))]),
            guard,
            kind,
        )
        .map_err(GuardedRequestError::into_message)?;
    parse_thread_state(result)
}

fn parse_thread_state(result: JsonValue) -> Result<ThreadState, String> {
    let status_type = result
        .get::<HashMap<String, JsonValue>>()
        .and_then(|result| result.get("thread"))
        .and_then(|thread| thread.get::<HashMap<String, JsonValue>>())
        .and_then(|thread| thread.get("status"))
        .and_then(|status| status.get::<HashMap<String, JsonValue>>())
        .and_then(|status| status.get("type"))
        .and_then(|kind| kind.get::<String>())
        .map(String::as_str)
        .ok_or_else(|| "thread/read response missing thread.status.type".to_string())?;
    match status_type {
        "idle" => Ok(ThreadState::Idle),
        "active" => Ok(ThreadState::Active),
        other => Err(format!(
            "thread is not ready for relay delivery (status {other})"
        )),
    }
}

fn start_ack_turn_with_guard(
    ws: &mut WsConn,
    id: u64,
    thread_id: &str,
    allow_bus: bool,
    service_tier: ServiceTier,
    guard: &mut ReentryGuard,
    kind: OperationKind,
) -> Result<(), String> {
    let result = ws
        .request_with_guard(
            id,
            "turn/start",
            turn_params(thread_id, service_tier),
            guard,
            kind,
        )
        .map_err(GuardedRequestError::into_message)?;
    let turn_id = turn_id_from_start_result(&result)?;
    let wait_ms: u64 = std::env::var("RELAY_TURN_WAIT_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_TURN_WAIT_MS);
    if !ws.pump_turn_with_guard(wait_ms, allow_bus, guard, thread_id, &turn_id)? {
        eprintln!(
            "[relay] turn on {thread_id} still running after {wait_ms}ms — detaching (a later MCP call may wedge it)"
        );
    }
    Ok(())
}

// ---- JSON-RPC param builders (pure — unit-tested shapes) ----

fn sobj(pairs: Vec<(&str, JsonValue)>) -> JsonValue {
    let mut m: HashMap<String, JsonValue> = HashMap::new();
    for (k, v) in pairs {
        m.insert(k.to_string(), v);
    }
    JsonValue::from(m)
}

fn inject_params(thread_id: &str, text: &str) -> JsonValue {
    sobj(vec![
        ("threadId", JsonValue::from(thread_id.to_string())),
        (
            "items",
            JsonValue::from(vec![sobj(vec![
                ("type", JsonValue::from("message".to_string())),
                ("role", JsonValue::from("user".to_string())),
                (
                    "content",
                    JsonValue::from(vec![sobj(vec![
                        ("type", JsonValue::from("input_text".to_string())),
                        ("text", JsonValue::from(text.to_string())),
                    ])]),
                ),
            ])]),
        ),
    ])
}

fn turn_params(thread_id: &str, service_tier: ServiceTier) -> JsonValue {
    sobj(vec![
        ("threadId", JsonValue::from(thread_id.to_string())),
        (
            "serviceTier",
            JsonValue::from(service_tier.as_str().to_string()),
        ),
        (
            "input",
            JsonValue::from(vec![sobj(vec![
                ("type", JsonValue::from("text".to_string())),
                ("text", JsonValue::from(ACK_NUDGE.to_string())),
            ])]),
        ),
        ("approvalPolicy", JsonValue::from("never".to_string())),
    ])
}

fn thread_start_params(
    cwd: &str,
    model: Option<&str>,
    sandbox: &str,
    service_tier: ServiceTier,
) -> JsonValue {
    let mut params: HashMap<String, JsonValue> = HashMap::new();
    params.insert("cwd".into(), JsonValue::from(cwd.to_string()));
    params.insert(
        "approvalPolicy".into(),
        JsonValue::from("never".to_string()),
    );
    params.insert("sandbox".into(), JsonValue::from(sandbox.to_string()));
    params.insert(
        "serviceTier".into(),
        JsonValue::from(service_tier.as_str().to_string()),
    );
    if let Some(model) = model {
        params.insert("model".into(), JsonValue::from(model.to_string()));
    }
    JsonValue::from(params)
}

fn initial_turn_params(
    thread_id: &str,
    prompt: &str,
    model: Option<&str>,
    effort: Option<&str>,
    service_tier: ServiceTier,
) -> JsonValue {
    let mut params = turn_params(thread_id, service_tier)
        .get::<HashMap<String, JsonValue>>()
        .cloned()
        .unwrap_or_default();
    params.insert(
        "input".into(),
        JsonValue::from(vec![sobj(vec![
            ("type", JsonValue::from("text".to_string())),
            ("text", JsonValue::from(prompt.to_string())),
        ])]),
    );
    if let Some(model) = model {
        params.insert("model".into(), JsonValue::from(model.to_string()));
    }
    if let Some(effort) = effort {
        params.insert("effort".into(), JsonValue::from(effort.to_string()));
    }
    JsonValue::from(params)
}

fn resume_params(thread_id: &str, service_tier: ServiceTier) -> JsonValue {
    sobj(vec![
        ("threadId", JsonValue::from(thread_id.to_string())),
        (
            "serviceTier",
            JsonValue::from(service_tier.as_str().to_string()),
        ),
    ])
}

fn verify_effective_service_tier(
    result: &JsonValue,
    requested: ServiceTier,
    method: &str,
) -> Result<(), String> {
    let reported = result
        .get::<HashMap<String, JsonValue>>()
        .and_then(|object| object.get("serviceTier"))
        .and_then(|value| value.get::<String>())
        .map(String::as_str)
        .ok_or_else(|| format!("{method} did not report an effective service tier"))?;
    if reported == requested.as_str() {
        Ok(())
    } else {
        Err(format!(
            "{method} requested {} but app-server reported {reported}",
            requested.as_str()
        ))
    }
}

// ---- minimal WebSocket client over a unix socket ----

fn urandom(n: usize) -> Vec<u8> {
    let mut f = std::fs::File::open("/dev/urandom")
        .unwrap_or_else(|e| die(&format!("open /dev/urandom: {e}")));
    let mut v = vec![0u8; n];
    f.read_exact(&mut v)
        .unwrap_or_else(|e| die(&format!("read /dev/urandom: {e}")));
    v
}

fn b64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn upgrade_request(key: &str) -> String {
    format!(
        "GET / HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
    )
}

// Client frames are always masked (RFC 6455 §5.3); server frames arrive
// unmasked but the parser tolerates either.
fn encode_client_frame(opcode: u8, payload: &[u8], mask: [u8; 4]) -> Vec<u8> {
    let mut f = vec![0x80 | (opcode & 0x0f)];
    let len = payload.len();
    if len < 126 {
        f.push(0x80 | len as u8);
    } else if len <= 0xFFFF {
        f.push(0x80 | 126);
        f.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        f.push(0x80 | 127);
        f.extend_from_slice(&(len as u64).to_be_bytes());
    }
    f.extend_from_slice(&mask);
    f.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    f
}

// (fin, opcode, payload, bytes consumed); None = incomplete frame, read more.
fn parse_frame(buf: &[u8]) -> Option<(bool, u8, Vec<u8>, usize)> {
    if buf.len() < 2 {
        return None;
    }
    let fin = buf[0] & 0x80 != 0;
    let opcode = buf[0] & 0x0f;
    let masked = buf[1] & 0x80 != 0;
    let mut len = (buf[1] & 0x7f) as usize;
    let mut i = 2;
    if len == 126 {
        if buf.len() < 4 {
            return None;
        }
        len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
        i = 4;
    } else if len == 127 {
        if buf.len() < 10 {
            return None;
        }
        len = u64::from_be_bytes(buf[2..10].try_into().ok()?) as usize;
        i = 10;
    }
    let mask: Option<[u8; 4]> = if masked {
        if buf.len() < i + 4 {
            return None;
        }
        let m = [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]];
        i += 4;
        Some(m)
    } else {
        None
    };
    if buf.len() < i + len {
        return None;
    }
    let mut payload = buf[i..i + len].to_vec();
    if let Some(m) = mask {
        for (k, b) in payload.iter_mut().enumerate() {
            *b ^= m[k % 4];
        }
    }
    Some((fin, opcode, payload, i + len))
}

struct WsConn {
    s: UnixStream,
    buf: Vec<u8>,
}

impl WsConn {
    fn connect(path: &str) -> Result<Self, String> {
        Self::connect_checked(path, || Ok(()))
    }

    fn connect_with_guard(
        path: &str,
        guard: &mut ReentryGuard,
        kind: OperationKind,
    ) -> Result<Self, String> {
        Self::connect_checked(path, || guard.authorize_use(kind).map(|_| ()))
    }

    fn connect_checked(
        path: &str,
        mut check: impl FnMut() -> Result<(), String>,
    ) -> Result<Self, String> {
        check()?;
        let mut s = UnixStream::connect(path).map_err(|e| format!("connect {path}: {e}"))?;
        s.set_read_timeout(Some(Duration::from_millis(100))).ok();
        let key = b64(&urandom(16));
        check()?;
        s.write_all(upgrade_request(&key).as_bytes())
            .map_err(|e| format!("upgrade write: {e}"))?;
        let mut hdr: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 512];
        let deadline = Instant::now() + Duration::from_secs(RPC_TIMEOUT_SECS);
        let end = loop {
            check()?;
            if Instant::now() >= deadline {
                return Err("app-server upgrade response timeout".to_string());
            }
            if let Some(pos) = hdr.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos + 4;
            }
            if hdr.len() > 16384 {
                return Err("oversized upgrade response".into());
            }
            let n = match s.read(&mut chunk) {
                Ok(n) => n,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        || error.kind() == std::io::ErrorKind::TimedOut =>
                {
                    continue;
                }
                Err(error) => return Err(format!("upgrade read: {error}")),
            };
            if n == 0 {
                return Err("connection closed during upgrade".into());
            }
            hdr.extend_from_slice(&chunk[..n]);
        };
        let head = String::from_utf8_lossy(&hdr[..end]);
        if !head.starts_with("HTTP/1.1 101") {
            return Err(format!(
                "upgrade refused: {}",
                head.lines().next().unwrap_or("")
            ));
        }
        Ok(WsConn {
            s,
            buf: hdr[end..].to_vec(),
        })
    }

    fn send_frame(&mut self, opcode: u8, payload: &[u8]) -> Result<(), String> {
        let mask: [u8; 4] = urandom(4).try_into().unwrap_or([0x5a; 4]);
        let frame = encode_client_frame(opcode, payload, mask);
        self.s
            .write_all(&frame)
            .map_err(|e| format!("ws write: {e}"))
    }

    // Ok(Some(text)) = one complete text message; Ok(None) = read timed out
    // (caller decides whether its own deadline has passed).
    fn recv_text(&mut self) -> Result<Option<String>, String> {
        let mut assembled: Vec<u8> = Vec::new();
        let mut in_text = false;
        loop {
            if let Some((fin, opcode, payload, used)) = parse_frame(&self.buf) {
                self.buf.drain(..used);
                match opcode {
                    0x1 => {
                        assembled = payload;
                        if fin {
                            return Ok(Some(String::from_utf8_lossy(&assembled).into_owned()));
                        }
                        in_text = true;
                    }
                    0x0 if in_text => {
                        assembled.extend_from_slice(&payload);
                        if fin {
                            return Ok(Some(String::from_utf8_lossy(&assembled).into_owned()));
                        }
                    }
                    0x9 => self.send_frame(0xA, &payload)?,
                    0x8 => return Err("server closed the connection".into()),
                    _ => {}
                }
                continue;
            }
            let mut chunk = [0u8; 4096];
            match self.s.read(&mut chunk) {
                Ok(0) => return Err("connection closed".into()),
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    return Ok(None);
                }
                Err(e) => return Err(format!("ws read: {e}")),
            }
        }
    }

    fn recv_text_with_guard(
        &mut self,
        guard: &mut ReentryGuard,
        kind: OperationKind,
    ) -> Result<Option<String>, String> {
        let mut assembled: Vec<u8> = Vec::new();
        let mut in_text = false;
        loop {
            guard.authorize_use(kind)?;
            if let Some((fin, opcode, payload, used)) = parse_frame(&self.buf) {
                self.buf.drain(..used);
                match opcode {
                    0x1 => {
                        assembled = payload;
                        if fin {
                            return Ok(Some(String::from_utf8_lossy(&assembled).into_owned()));
                        }
                        in_text = true;
                    }
                    0x0 if in_text => {
                        assembled.extend_from_slice(&payload);
                        if fin {
                            return Ok(Some(String::from_utf8_lossy(&assembled).into_owned()));
                        }
                    }
                    0x9 => {
                        guard.authorize_use(kind)?;
                        self.send_frame(0xA, &payload)?;
                    }
                    0x8 => return Err("server closed the connection".into()),
                    _ => {}
                }
                continue;
            }
            let mut chunk = [0u8; 4096];
            match self.s.read(&mut chunk) {
                Ok(0) => return Err("connection closed".into()),
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        || error.kind() == std::io::ErrorKind::TimedOut =>
                {
                    return Ok(None);
                }
                Err(error) => return Err(format!("ws read: {error}")),
            }
        }
    }

    fn notify(&mut self, method: &str, params: JsonValue) -> Result<(), String> {
        let msg = sobj(vec![
            ("method", JsonValue::from(method.to_string())),
            ("params", params),
        ]);
        self.send_frame(0x1, msg.stringify().map_err(|e| format!("{e}"))?.as_bytes())
    }

    fn notify_with_guard(
        &mut self,
        method: &str,
        params: JsonValue,
        guard: &mut ReentryGuard,
        kind: OperationKind,
    ) -> Result<(), String> {
        let msg = sobj(vec![
            ("method", JsonValue::from(method.to_string())),
            ("params", params),
        ]);
        guard.authorize_use(kind)?;
        self.send_frame(0x1, msg.stringify().map_err(|e| format!("{e}"))?.as_bytes())
    }

    fn request(&mut self, id: u64, method: &str, params: JsonValue) -> Result<JsonValue, String> {
        let msg = sobj(vec![
            ("id", JsonValue::from(id as f64)),
            ("method", JsonValue::from(method.to_string())),
            ("params", params),
        ]);
        self.send_frame(0x1, msg.stringify().map_err(|e| format!("{e}"))?.as_bytes())?;
        let deadline = Instant::now() + Duration::from_secs(RPC_TIMEOUT_SECS);
        loop {
            if Instant::now() > deadline {
                return Err(format!("{method}: response timeout"));
            }
            let Some(text) = self.recv_text()? else {
                continue;
            };
            let Ok(v) = text.parse::<JsonValue>() else {
                continue;
            };
            let Some(o) = v.get::<HashMap<String, JsonValue>>() else {
                continue;
            };
            let got_id = o.get("id").and_then(|x| x.get::<f64>().copied());
            if got_id != Some(id as f64) {
                continue;
            }
            if let Some(err) = o.get("error") {
                let emsg = err
                    .get::<HashMap<String, JsonValue>>()
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.get::<String>().cloned())
                    .unwrap_or_else(|| "unknown error".into());
                return Err(format!("{method}: {emsg}"));
            }
            return Ok(o.get("result").cloned().unwrap_or(JsonValue::from(())));
        }
    }

    fn request_with_guard(
        &mut self,
        id: u64,
        method: &str,
        params: JsonValue,
        guard: &mut ReentryGuard,
        kind: OperationKind,
    ) -> Result<JsonValue, GuardedRequestError> {
        let msg = sobj(vec![
            ("id", JsonValue::from(id as f64)),
            ("method", JsonValue::from(method.to_string())),
            ("params", params),
        ]);
        let encoded = msg
            .stringify()
            .map_err(|error| GuardedRequestError::BeforeSend(format!("{error}")))?;
        guard
            .authorize_use(kind)
            .map_err(GuardedRequestError::BeforeSend)?;
        self.send_frame(0x1, encoded.as_bytes())
            .map_err(GuardedRequestError::AfterSend)?;
        let deadline = Instant::now() + Duration::from_secs(RPC_TIMEOUT_SECS);
        loop {
            if guard.cancelled() {
                return Err(GuardedRequestError::AfterSend(format!(
                    "{method}: cancelled"
                )));
            }
            if Instant::now() > deadline {
                return Err(GuardedRequestError::AfterSend(format!(
                    "{method}: response timeout"
                )));
            }
            let Some(text) = self
                .recv_text_with_guard(guard, kind)
                .map_err(GuardedRequestError::AfterSend)?
            else {
                continue;
            };
            let Ok(v) = text.parse::<JsonValue>() else {
                continue;
            };
            let Some(o) = v.get::<HashMap<String, JsonValue>>() else {
                continue;
            };
            let got_id = o.get("id").and_then(|x| x.get::<f64>().copied());
            if got_id != Some(id as f64) {
                continue;
            }
            if let Some(err) = o.get("error") {
                let emsg = err
                    .get::<HashMap<String, JsonValue>>()
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.get::<String>().cloned())
                    .unwrap_or_else(|| "unknown error".into());
                return Err(GuardedRequestError::AfterSend(format!("{method}: {emsg}")));
            }
            return Ok(o.get("result").cloned().unwrap_or(JsonValue::from(())));
        }
    }

    fn pump_turn_with_guard(
        &mut self,
        ms: u64,
        allow_bus: bool,
        guard: &mut ReentryGuard,
        expected_thread_id: &str,
        expected_turn_id: &str,
    ) -> Result<bool, String> {
        let deadline = Instant::now() + Duration::from_millis(ms);
        loop {
            if guard.cancelled() {
                return Err("app-server turn pump cancelled".to_string());
            }
            if Instant::now() > deadline {
                return Ok(false);
            }
            let kind = guard.allowed();
            let Some(text) = self.recv_text_with_guard(guard, kind)? else {
                continue;
            };
            let Ok(value) = text.parse::<JsonValue>() else {
                continue;
            };
            let Some(object) = value.get::<HashMap<String, JsonValue>>() else {
                continue;
            };
            let method = object
                .get("method")
                .and_then(|value| value.get::<String>().cloned());
            match method.as_deref() {
                Some("turn/completed")
                    if turn_event_identity(object).as_ref()
                        == Some(&(
                            expected_thread_id.to_string(),
                            expected_turn_id.to_string(),
                        )) =>
                {
                    return Ok(true);
                }
                Some("mcpServer/elicitation/request") => {
                    let Some(request_id) = object.get("id").cloned() else {
                        continue;
                    };
                    let server_name = object
                        .get("params")
                        .and_then(|value| value.get::<HashMap<String, JsonValue>>())
                        .and_then(|params| params.get("serverName"))
                        .and_then(|value| value.get::<String>().cloned())
                        .unwrap_or_default();
                    let action = elicitation_action(&server_name, allow_bus);
                    let message = sobj(vec![
                        ("id", request_id),
                        (
                            "result",
                            sobj(vec![("action", JsonValue::from(action.to_string()))]),
                        ),
                    ]);
                    guard.authorize_use(guard.allowed())?;
                    self.send_frame(
                        0x1,
                        message
                            .stringify()
                            .map_err(|error| format!("{error}"))?
                            .as_bytes(),
                    )?;
                }
                _ => {}
            }
        }
    }

    fn interrupt_and_confirm(
        &mut self,
        id: u64,
        thread_id: &str,
        turn_id: &str,
        timeout_ms: u64,
    ) -> Result<bool, String> {
        let message = sobj(vec![
            ("id", JsonValue::from(id as f64)),
            ("method", JsonValue::from("turn/interrupt".to_string())),
            (
                "params",
                sobj(vec![
                    ("threadId", JsonValue::from(thread_id.to_string())),
                    ("turnId", JsonValue::from(turn_id.to_string())),
                ]),
            ),
        ]);
        self.send_frame(
            0x1,
            message
                .stringify()
                .map_err(|error| format!("turn/interrupt: {error}"))?
                .as_bytes(),
        )?;
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let mut response_seen = false;
        let mut completion_seen = false;
        while Instant::now() <= deadline {
            let Some(text) = self.recv_text()? else {
                continue;
            };
            let Ok(value) = text.parse::<JsonValue>() else {
                continue;
            };
            let Some(object) = value.get::<HashMap<String, JsonValue>>() else {
                continue;
            };
            if object
                .get("id")
                .and_then(|value| value.get::<f64>())
                .copied()
                == Some(id as f64)
            {
                if let Some(error) = object.get("error") {
                    let message = error
                        .get::<HashMap<String, JsonValue>>()
                        .and_then(|error| error.get("message"))
                        .and_then(|message| message.get::<String>())
                        .cloned()
                        .unwrap_or_else(|| "unknown error".to_string());
                    return Err(format!("turn/interrupt: {message}"));
                }
                let result = object
                    .get("result")
                    .and_then(|result| result.get::<HashMap<String, JsonValue>>())
                    .ok_or_else(|| "turn/interrupt response was not an object".to_string())?;
                if !result.is_empty() {
                    return Err("turn/interrupt response was not empty".to_string());
                }
                response_seen = true;
            } else if object.get("method").and_then(|value| value.get::<String>())
                == Some(&"turn/completed".to_string())
                && turn_event_identity(object).as_ref()
                    == Some(&(thread_id.to_string(), turn_id.to_string()))
            {
                completion_seen = true;
            }
            if response_seen && completion_seen {
                return Ok(true);
            }
        }
        if !response_seen {
            return Err("turn/interrupt response timeout".to_string());
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn b64_matches_known_vectors() {
        assert_eq!(b64(b"abc"), "YWJj");
        assert_eq!(b64(b"ab"), "YWI=");
        assert_eq!(b64(b"a"), "YQ==");
        assert_eq!(b64(&[0u8; 16]).len(), 24);
    }

    #[test]
    fn frame_roundtrip_small_and_extended_lengths() {
        for payload in [&b"hello"[..], &[0x42u8; 300][..]] {
            let f = encode_client_frame(0x1, payload, [1, 2, 3, 4]);
            let (fin, opcode, decoded, used) = parse_frame(&f).expect("complete frame");
            assert!(fin);
            assert_eq!(opcode, 0x1);
            assert_eq!(decoded, payload);
            assert_eq!(used, f.len());
        }
        assert!(parse_frame(&[0x81]).is_none());
    }

    #[test]
    fn elicitations_accept_the_relay_bus_only_for_relay_owned_threads() {
        assert_eq!(elicitation_action("bus", true), "accept");
        assert_eq!(elicitation_action("bus", false), "decline");
        assert_eq!(elicitation_action("codex_apps", true), "decline");
        assert_eq!(elicitation_action("", true), "decline");
    }

    #[test]
    fn upgrade_request_carries_the_ws_headers() {
        let r = upgrade_request("KEY123");
        assert!(r.contains("Upgrade: websocket"));
        assert!(r.contains("Sec-WebSocket-Key: KEY123"));
        assert!(r.contains("Sec-WebSocket-Version: 13"));
        assert!(r.ends_with("\r\n\r\n"));
    }

    fn as_obj(v: &JsonValue) -> &HashMap<String, JsonValue> {
        v.get::<HashMap<String, JsonValue>>().expect("object")
    }

    #[test]
    fn inject_params_wrap_the_fenced_text_as_a_user_input_item() {
        let p = inject_params("tid-1", "<session-relay-mail>hi</session-relay-mail>");
        let o = as_obj(&p);
        assert_eq!(o.get("threadId").unwrap().get::<String>().unwrap(), "tid-1");
        let items = o.get("items").unwrap().get::<Vec<JsonValue>>().unwrap();
        let item = as_obj(&items[0]);
        assert_eq!(item.get("role").unwrap().get::<String>().unwrap(), "user");
        let content = item
            .get("content")
            .unwrap()
            .get::<Vec<JsonValue>>()
            .unwrap();
        let c0 = as_obj(&content[0]);
        assert_eq!(
            c0.get("type").unwrap().get::<String>().unwrap(),
            "input_text"
        );
        assert!(
            c0.get("text")
                .unwrap()
                .get::<String>()
                .unwrap()
                .contains("session-relay-mail")
        );
    }

    #[test]
    fn turn_params_use_the_neutral_nudge_and_never_approvals() {
        let p = turn_params("tid-2", crate::lifecycle::ServiceTier::Default);
        let o = as_obj(&p);
        assert_eq!(
            o.get("approvalPolicy").unwrap().get::<String>().unwrap(),
            "never"
        );
        let input = o.get("input").unwrap().get::<Vec<JsonValue>>().unwrap();
        let text = as_obj(&input[0])
            .get("text")
            .unwrap()
            .get::<String>()
            .cloned()
            .unwrap();
        assert_eq!(text, ACK_NUDGE);
        assert!(!text.to_ascii_lowercase().contains("call inbox"));
        assert!(!text.contains("session-relay-mail"));
        assert_eq!(
            o.get("serviceTier").unwrap().get::<String>().unwrap(),
            "default"
        );
    }

    #[test]
    fn start_and_turn_params_always_carry_the_explicit_service_tier() {
        let standard = thread_start_params(
            "/tmp/project",
            Some("gpt-5.6-sol"),
            "workspace-write",
            crate::lifecycle::ServiceTier::Default,
        );
        assert_eq!(
            as_obj(&standard)
                .get("serviceTier")
                .unwrap()
                .get::<String>()
                .unwrap(),
            "default"
        );

        let fast = initial_turn_params(
            "tid-fast",
            "implement",
            Some("gpt-5.6-sol"),
            Some("high"),
            crate::lifecycle::ServiceTier::Fast,
        );
        assert_eq!(
            as_obj(&fast)
                .get("serviceTier")
                .unwrap()
                .get::<String>()
                .unwrap(),
            "fast"
        );
    }

    #[test]
    fn effective_service_tier_verification_fails_closed_on_missing_or_mismatch() {
        let matching = r#"{"serviceTier":"fast"}"#.parse::<JsonValue>().unwrap();
        assert!(
            verify_effective_service_tier(
                &matching,
                crate::lifecycle::ServiceTier::Fast,
                "thread/start"
            )
            .is_ok()
        );

        let missing = r#"{"thread":{"id":"tid"}}"#.parse::<JsonValue>().unwrap();
        assert!(
            verify_effective_service_tier(
                &missing,
                crate::lifecycle::ServiceTier::Fast,
                "thread/start"
            )
            .unwrap_err()
            .contains("did not report an effective service tier")
        );

        let mismatch = r#"{"serviceTier":"default"}"#.parse::<JsonValue>().unwrap();
        assert!(
            verify_effective_service_tier(
                &mismatch,
                crate::lifecycle::ServiceTier::Fast,
                "thread/resume"
            )
            .unwrap_err()
            .contains("requested fast but app-server reported default")
        );
    }
}
