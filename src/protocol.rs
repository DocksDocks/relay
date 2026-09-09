use crate::jcs::{
    ClosedJcs, JcsValue, LowerUuidV4, Sha256Digest, parse_jcs, read_jcs_file, serialize_jcs,
};
use crate::sha256;
use crate::store;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use tinyjson::JsonValue;

const MESSAGE_KEYS: [&str; 11] = [
    "schema",
    "id",
    "created_at",
    "from_session_id",
    "to_session_id",
    "correlation_id",
    "kind",
    "reply_to",
    "terminal_status",
    "body",
    "result_sha256",
];
const CLAIM_KEYS: [&str; 14] = [
    "schema",
    "correlation_id",
    "origin",
    "state",
    "requester_session_id",
    "responder_session_id",
    "request_sha256",
    "request",
    "request_delivery",
    "reply",
    "reply_sha256",
    "reply_delivery",
    "created_at",
    "updated_at",
];

fn object(entries: impl IntoIterator<Item = (&'static str, JcsValue)>) -> JcsValue {
    JcsValue::Object(
        entries
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect(),
    )
}

fn require_keys(values: &BTreeMap<String, JcsValue>, expected: &[&str]) -> Result<(), String> {
    let actual: BTreeSet<_> = values.keys().map(String::as_str).collect();
    let expected: BTreeSet<_> = expected.iter().copied().collect();
    if actual == expected {
        Ok(())
    } else {
        Err(format!(
            "closed protocol keys differ: expected {expected:?}, got {actual:?}"
        ))
    }
}

fn string(values: &BTreeMap<String, JcsValue>, key: &str) -> Result<String, String> {
    values
        .get(key)
        .ok_or_else(|| format!("missing {key}"))?
        .as_str()
        .map(str::to_string)
}

fn optional_string(
    values: &BTreeMap<String, JcsValue>,
    key: &str,
) -> Result<Option<String>, String> {
    match values.get(key).ok_or_else(|| format!("missing {key}"))? {
        JcsValue::Null => Ok(None),
        JcsValue::String(value) => Ok(Some(value.clone())),
        _ => Err(format!("{key} must be a string or null")),
    }
}

fn schema(values: &BTreeMap<String, JcsValue>, expected: i64) -> Result<u8, String> {
    match values.get("schema") {
        Some(JcsValue::Integer(value)) if *value == expected => Ok(expected as u8),
        _ => Err(format!("protocol schema must be integer {expected}")),
    }
}

fn optional_record<T: ClosedJcs>(
    values: &BTreeMap<String, JcsValue>,
    key: &str,
) -> Result<Option<T>, String> {
    match values.get(key).ok_or_else(|| format!("missing {key}"))? {
        JcsValue::Null => Ok(None),
        value => T::from_jcs(value.clone()).map(Some),
    }
}

fn canonical_bytes<T: ClosedJcs>(value: &T) -> Vec<u8> {
    serialize_jcs(&value.to_jcs()).into_bytes()
}

fn digest<T: ClosedJcs>(value: &T) -> String {
    sha256::hex_digest(&canonical_bytes(value))
}

fn valid_timestamp(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 24
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[19] != b'.'
        || bytes[23] != b'Z'
        || bytes.iter().enumerate().any(|(index, byte)| {
            ![4, 7, 10, 13, 16, 19, 23].contains(&index) && !byte.is_ascii_digit()
        })
    {
        return false;
    }
    let number = |start: usize, end: usize| {
        std::str::from_utf8(&bytes[start..end])
            .ok()?
            .parse::<u32>()
            .ok()
    };
    let (Some(year), Some(month), Some(day), Some(hour), Some(minute), Some(second)) = (
        number(0, 4),
        number(5, 7),
        number(8, 10),
        number(11, 13),
        number(14, 16),
        number(17, 19),
    ) else {
        return false;
    };
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    (1..=days).contains(&day) && hour <= 23 && minute <= 59 && second <= 59
}

fn validate_uuid(value: &str, label: &str) -> Result<(), String> {
    LowerUuidV4::parse(value)
        .map(|_| ())
        .map_err(|_| format!("{label} is not a lowercase UUID v4"))
}

/// Omp runtime session ids may be UUIDv7. Accept any lowercase UUID shape;
/// relay-generated ids stay strict v4.
fn validate_session_id(value: &str, label: &str) -> Result<(), String> {
    if store::is_uuid(value) && !value.bytes().any(|b| b.is_ascii_uppercase()) {
        Ok(())
    } else {
        Err(format!("{label} is not a lowercase UUID"))
    }
}

fn validate_timestamp(value: &str, label: &str) -> Result<(), String> {
    if valid_timestamp(value) {
        Ok(())
    } else {
        Err(format!(
            "{label} is not an exact real millisecond UTC instant"
        ))
    }
}

fn validate_sha(value: &str, label: &str) -> Result<(), String> {
    Sha256Digest::parse(value)
        .map(|_| ())
        .map_err(|_| format!("{label} is not a lowercase SHA-256"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MessageKind {
    Request,
    TerminalReply,
}

impl MessageKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Request => "request",
            Self::TerminalReply => "terminal_reply",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "request" => Ok(Self::Request),
            "terminal_reply" => Ok(Self::TerminalReply),
            _ => Err("unknown MessageV2 kind".to_string()),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalStatus {
    Completed,
    Failed,
}

impl TerminalStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            _ => Err("unknown terminal status".to_string()),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MessageV2 {
    pub schema: u8,
    pub id: String,
    pub created_at: String,
    pub from_session_id: String,
    pub to_session_id: String,
    pub correlation_id: String,
    pub kind: MessageKind,
    pub reply_to: Option<String>,
    pub terminal_status: Option<TerminalStatus>,
    pub body: String,
    pub result_sha256: Option<String>,
}

impl MessageV2 {
    pub fn canonical_bytes(&self) -> Vec<u8> {
        canonical_bytes(self)
    }

    pub fn sha256(&self) -> String {
        digest(self)
    }

    pub fn from_tinyjson(value: &JsonValue) -> Result<Self, String> {
        Self::from_jcs(jcs_from_tinyjson(value)?)
    }

    fn validate(&self) -> Result<(), String> {
        if self.schema != 2 {
            return Err("MessageV2 schema mismatch".to_string());
        }
        for (label, value) in [
            ("message id", self.id.as_str()),
            ("correlation id", self.correlation_id.as_str()),
        ] {
            validate_uuid(value, label)?;
        }
        for (label, value) in [
            ("from session id", self.from_session_id.as_str()),
            ("to session id", self.to_session_id.as_str()),
        ] {
            validate_session_id(value, label)?;
        }
        if let Some(value) = &self.reply_to {
            validate_uuid(value, "reply_to")?;
        }
        validate_timestamp(&self.created_at, "message created_at")?;
        if self.body.is_empty() || self.body.len() > 4096 || self.body.contains('\0') {
            return Err("message body is outside the UTF-8 byte bounds".to_string());
        }
        if let Some(value) = &self.result_sha256 {
            validate_sha(value, "result_sha256")?;
        }
        let legal = match self.kind {
            MessageKind::Request => {
                self.reply_to.is_none()
                    && self.terminal_status.is_none()
                    && self.result_sha256.is_none()
            }
            MessageKind::TerminalReply => {
                self.reply_to.is_some()
                    && self.terminal_status.is_some()
                    && self.result_sha256.is_none()
            }
        };
        if !legal {
            return Err("MessageV2 variant matrix violation".to_string());
        }
        if serialize_jcs(&self.to_jcs()).len() > 16 * 1024 {
            return Err("MessageV2 exceeds encoded envelope limit".to_string());
        }
        Ok(())
    }
}

impl ClosedJcs for MessageV2 {
    fn from_jcs(value: JcsValue) -> Result<Self, String> {
        let values = value.object()?;
        require_keys(&values, &MESSAGE_KEYS)?;
        let message = Self {
            schema: schema(&values, 2)?,
            id: string(&values, "id")?,
            created_at: string(&values, "created_at")?,
            from_session_id: string(&values, "from_session_id")?,
            to_session_id: string(&values, "to_session_id")?,
            correlation_id: string(&values, "correlation_id")?,
            kind: MessageKind::parse(&string(&values, "kind")?)?,
            reply_to: optional_string(&values, "reply_to")?,
            terminal_status: optional_string(&values, "terminal_status")?
                .map(|value| TerminalStatus::parse(&value))
                .transpose()?,
            body: string(&values, "body")?,
            result_sha256: optional_string(&values, "result_sha256")?,
        };
        message.validate()?;
        Ok(message)
    }

    fn to_jcs(&self) -> JcsValue {
        object([
            ("body", JcsValue::String(self.body.clone())),
            (
                "correlation_id",
                JcsValue::String(self.correlation_id.clone()),
            ),
            ("created_at", JcsValue::String(self.created_at.clone())),
            (
                "from_session_id",
                JcsValue::String(self.from_session_id.clone()),
            ),
            ("id", JcsValue::String(self.id.clone())),
            ("kind", JcsValue::String(self.kind.as_str().into())),
            (
                "reply_to",
                self.reply_to
                    .clone()
                    .map(JcsValue::String)
                    .unwrap_or(JcsValue::Null),
            ),
            (
                "result_sha256",
                self.result_sha256
                    .clone()
                    .map(JcsValue::String)
                    .unwrap_or(JcsValue::Null),
            ),
            ("schema", JcsValue::Integer(i64::from(self.schema))),
            (
                "terminal_status",
                self.terminal_status
                    .map(|value| JcsValue::String(value.as_str().into()))
                    .unwrap_or(JcsValue::Null),
            ),
            (
                "to_session_id",
                JcsValue::String(self.to_session_id.clone()),
            ),
        ])
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClaimOrigin {
    Message,
}

impl ClaimOrigin {
    fn as_str(self) -> &'static str {
        match self {
            Self::Message => "message",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "message" => Ok(Self::Message),
            _ => Err("unknown claim origin".to_string()),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClaimState {
    RequestPending,
    Open,
    ReplyPending,
    ReplyEnqueued,
    ReplyConsumed,
}

impl ClaimState {
    fn as_str(self) -> &'static str {
        match self {
            Self::RequestPending => "RequestPending",
            Self::Open => "Open",
            Self::ReplyPending => "ReplyPending",
            Self::ReplyEnqueued => "ReplyEnqueued",
            Self::ReplyConsumed => "ReplyConsumed",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "RequestPending" => Ok(Self::RequestPending),
            "Open" => Ok(Self::Open),
            "ReplyPending" => Ok(Self::ReplyPending),
            "ReplyEnqueued" => Ok(Self::ReplyEnqueued),
            "ReplyConsumed" => Ok(Self::ReplyConsumed),
            _ => Err("unknown claim state".to_string()),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeliveryState {
    Pending,
    Enqueued,
    Consumed,
    NotApplicable,
}

impl DeliveryState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Enqueued => "enqueued",
            Self::Consumed => "consumed",
            Self::NotApplicable => "not_applicable",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "pending" => Ok(Self::Pending),
            "enqueued" => Ok(Self::Enqueued),
            "consumed" => Ok(Self::Consumed),
            "not_applicable" => Ok(Self::NotApplicable),
            _ => Err("unknown delivery state".to_string()),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClaimStatusV1 {
    pub schema: u8,
    pub correlation_id: String,
    pub origin: ClaimOrigin,
    pub state: ClaimState,
    pub requester_session_id: String,
    pub responder_session_id: String,
    pub request_sha256: String,
    pub request: MessageV2,
    pub request_delivery: DeliveryState,
    pub reply: Option<MessageV2>,
    pub reply_sha256: Option<String>,
    pub reply_delivery: Option<DeliveryState>,
    pub created_at: String,
    pub updated_at: String,
}

impl ClaimStatusV1 {
    pub fn canonical_bytes(&self) -> Vec<u8> {
        canonical_bytes(self)
    }

    pub fn sha256(&self) -> String {
        digest(self)
    }

    fn validate(&self) -> Result<(), String> {
        if self.schema != 1 {
            return Err("ClaimStatusV1 schema mismatch".to_string());
        }
        validate_uuid(&self.correlation_id, "claim correlation id")?;
        for (label, value) in [
            ("claim requester", self.requester_session_id.as_str()),
            ("claim responder", self.responder_session_id.as_str()),
        ] {
            validate_session_id(value, label)?;
        }
        validate_sha(&self.request_sha256, "request_sha256")?;
        if let Some(value) = &self.reply_sha256 {
            validate_sha(value, "reply_sha256")?;
        }
        validate_timestamp(&self.created_at, "claim created_at")?;
        validate_timestamp(&self.updated_at, "claim updated_at")?;
        self.request.validate()?;
        if self.request.kind != MessageKind::Request
            || self.request.correlation_id != self.correlation_id
            || self.request.from_session_id != self.requester_session_id
            || self.request.to_session_id != self.responder_session_id
            || self.request.sha256() != self.request_sha256
        {
            return Err("claim request authority binding mismatch".to_string());
        }
        let has_reply = self.reply.is_some();
        let legal_request_delivery = matches!(
            self.request_delivery,
            DeliveryState::Enqueued | DeliveryState::Consumed
        );
        let legal = match self.state {
            ClaimState::RequestPending => {
                self.request_delivery == DeliveryState::Pending
                    && !has_reply
                    && self.reply_delivery.is_none()
            }
            ClaimState::Open => {
                legal_request_delivery && !has_reply && self.reply_delivery.is_none()
            }
            ClaimState::ReplyPending => {
                legal_request_delivery
                    && has_reply
                    && self.reply_delivery == Some(DeliveryState::Pending)
            }
            ClaimState::ReplyEnqueued => {
                legal_request_delivery
                    && has_reply
                    && self.reply_delivery == Some(DeliveryState::Enqueued)
            }
            ClaimState::ReplyConsumed => {
                legal_request_delivery
                    && has_reply
                    && self.reply_delivery == Some(DeliveryState::Consumed)
            }
        };
        if !legal {
            return Err("claim origin/state/delivery matrix violation".to_string());
        }
        match (&self.reply, &self.reply_sha256) {
            (None, None) => {}
            (Some(reply), Some(reply_sha256)) => {
                reply.validate()?;
                if reply.kind != MessageKind::TerminalReply
                    || reply.correlation_id != self.correlation_id
                    || reply.from_session_id != self.responder_session_id
                    || reply.to_session_id != self.requester_session_id
                    || reply.reply_to.as_deref() != Some(self.request.id.as_str())
                    || reply.sha256() != *reply_sha256
                {
                    return Err("claim reply authority binding mismatch".to_string());
                }
            }
            _ => return Err("claim reply and digest presence mismatch".to_string()),
        }
        Ok(())
    }
}

impl ClosedJcs for ClaimStatusV1 {
    fn from_jcs(value: JcsValue) -> Result<Self, String> {
        let values = value.object()?;
        require_keys(&values, &CLAIM_KEYS)?;
        let claim = Self {
            schema: schema(&values, 1)?,
            correlation_id: string(&values, "correlation_id")?,
            origin: ClaimOrigin::parse(&string(&values, "origin")?)?,
            state: ClaimState::parse(&string(&values, "state")?)?,
            requester_session_id: string(&values, "requester_session_id")?,
            responder_session_id: string(&values, "responder_session_id")?,
            request_sha256: string(&values, "request_sha256")?,
            request: MessageV2::from_jcs(
                values
                    .get("request")
                    .ok_or_else(|| "missing request".to_string())?
                    .clone(),
            )?,
            request_delivery: DeliveryState::parse(&string(&values, "request_delivery")?)?,
            reply: optional_record(&values, "reply")?,
            reply_sha256: optional_string(&values, "reply_sha256")?,
            reply_delivery: optional_string(&values, "reply_delivery")?
                .map(|value| DeliveryState::parse(&value))
                .transpose()?,
            created_at: string(&values, "created_at")?,
            updated_at: string(&values, "updated_at")?,
        };
        claim.validate()?;
        Ok(claim)
    }

    fn to_jcs(&self) -> JcsValue {
        object([
            (
                "correlation_id",
                JcsValue::String(self.correlation_id.clone()),
            ),
            ("created_at", JcsValue::String(self.created_at.clone())),
            ("origin", JcsValue::String(self.origin.as_str().into())),
            (
                "reply",
                self.reply
                    .as_ref()
                    .map(ClosedJcs::to_jcs)
                    .unwrap_or(JcsValue::Null),
            ),
            (
                "reply_delivery",
                self.reply_delivery
                    .map(|value| JcsValue::String(value.as_str().into()))
                    .unwrap_or(JcsValue::Null),
            ),
            (
                "reply_sha256",
                self.reply_sha256
                    .clone()
                    .map(JcsValue::String)
                    .unwrap_or(JcsValue::Null),
            ),
            ("request", self.request.to_jcs()),
            (
                "request_delivery",
                JcsValue::String(self.request_delivery.as_str().into()),
            ),
            (
                "request_sha256",
                JcsValue::String(self.request_sha256.clone()),
            ),
            (
                "requester_session_id",
                JcsValue::String(self.requester_session_id.clone()),
            ),
            (
                "responder_session_id",
                JcsValue::String(self.responder_session_id.clone()),
            ),
            ("schema", JcsValue::Integer(i64::from(self.schema))),
            ("state", JcsValue::String(self.state.as_str().into())),
            ("updated_at", JcsValue::String(self.updated_at.clone())),
        ])
    }
}

pub fn jcs_from_tinyjson(value: &JsonValue) -> Result<JcsValue, String> {
    if value.is_null() {
        return Ok(JcsValue::Null);
    }
    if let Some(value) = value.get::<bool>() {
        return Ok(JcsValue::Bool(*value));
    }
    if let Some(value) = value.get::<String>() {
        return Ok(JcsValue::String(value.clone()));
    }
    if let Some(value) = value.get::<f64>() {
        if value.is_finite()
            && value.fract() == 0.0
            && *value >= i64::MIN as f64
            && *value <= i64::MAX as f64
        {
            return Ok(JcsValue::Integer(*value as i64));
        }
        return Err("protocol JSON number is not an integer".to_string());
    }
    if let Some(values) = value.get::<Vec<JsonValue>>() {
        return values
            .iter()
            .map(jcs_from_tinyjson)
            .collect::<Result<Vec<_>, _>>()
            .map(JcsValue::Array);
    }
    if let Some(values) = value.get::<HashMap<String, JsonValue>>() {
        return values
            .iter()
            .map(|(key, value)| Ok((key.clone(), jcs_from_tinyjson(value)?)))
            .collect::<Result<BTreeMap<_, _>, String>>()
            .map(JcsValue::Object);
    }
    Err("unsupported protocol JSON value".to_string())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplyDisposition {
    Created,
    Idempotent,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplyOutcome {
    pub disposition: ReplyDisposition,
    pub message: MessageV2,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProtocolFailpoint {
    RequestBeforePendingWrite,
    RequestAfterPendingWrite,
    RequestBeforeMailboxAppend,
    RequestAfterMailboxAppend,
    RequestBeforeOpenMove,
    RequestOpenMoveBeforeSourceUnlink,
    RequestAfterOpenMove,
    ReplyBeforePendingWrite,
    ReplyPendingMoveBeforeSourceUnlink,
    ReplyAfterPendingWrite,
    ReplyBeforeMailboxAppend,
    ReplyAfterMailboxAppend,
    ReplyBeforeEnqueuedMove,
    ReplyEnqueuedMoveBeforeSourceUnlink,
    ReplyAfterEnqueuedMove,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProtocolError {
    UnknownCorrelation,
    UnauthorizedResponder,
    CorrelationConflict,
    ProtocolStoreError(String),
}

impl ProtocolError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnknownCorrelation => "unknown_correlation",
            Self::UnauthorizedResponder => "unauthorized_responder",
            Self::CorrelationConflict => "correlation_conflict",
            Self::ProtocolStoreError(_) => "protocol_store_error",
        }
    }

    fn store(error: impl fmt::Display) -> Self {
        Self::ProtocolStoreError(error.to_string())
    }
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownCorrelation | Self::UnauthorizedResponder | Self::CorrelationConflict => {
                formatter.write_str(self.code())
            }
            Self::ProtocolStoreError(error) => write!(formatter, "{}: {error}", self.code()),
        }
    }
}

impl std::error::Error for ProtocolError {}

#[derive(Clone, Copy)]
enum ClaimDirectory {
    Pending,
    Open,
    Terminal,
}

impl ClaimDirectory {
    fn name(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Open => "open",
            Self::Terminal => "terminal",
        }
    }
}

/// Identity fields that never change across a claim's lifetime. Two files for
/// one correlation must agree on all of them before the earlier state may be
/// treated as the stale source of an interrupted `move_claim`.
fn same_claim_identity(a: &ClaimStatusV1, b: &ClaimStatusV1) -> bool {
    a.schema == b.schema
        && a.correlation_id == b.correlation_id
        && a.origin == b.origin
        && a.requester_session_id == b.requester_session_id
        && a.responder_session_id == b.responder_session_id
        && a.request == b.request
        && a.request_sha256 == b.request_sha256
        && a.created_at == b.created_at
}

/// pending/RequestPending is the stale source of an interrupted
/// Pending -> Open request move whose destination `authoritative` survived.
fn stale_request_pending_of_open(stale: &ClaimStatusV1, authoritative: &ClaimStatusV1) -> bool {
    same_claim_identity(stale, authoritative)
        && stale.state == ClaimState::RequestPending
        && stale.request_delivery == DeliveryState::Pending
        && stale.reply.is_none()
        && authoritative.state == ClaimState::Open
        && matches!(
            authoritative.request_delivery,
            DeliveryState::Enqueued | DeliveryState::Consumed
        )
        && authoritative.reply.is_none()
}

/// open/Open is the stale source of an interrupted Open -> Pending reply move.
fn stale_open_of_reply_pending(stale: &ClaimStatusV1, authoritative: &ClaimStatusV1) -> bool {
    same_claim_identity(stale, authoritative)
        && stale.state == ClaimState::Open
        && stale.reply.is_none()
        && authoritative.state == ClaimState::ReplyPending
        && authoritative.request_delivery == stale.request_delivery
        && authoritative.reply.is_some()
        && authoritative.reply_delivery == Some(DeliveryState::Pending)
}

/// pending/ReplyPending is the stale source of an interrupted
/// Pending -> Terminal reply move whose destination `authoritative` survived.
fn stale_reply_pending_of_terminal(stale: &ClaimStatusV1, authoritative: &ClaimStatusV1) -> bool {
    same_claim_identity(stale, authoritative)
        && stale.state == ClaimState::ReplyPending
        && stale.reply.is_some()
        && stale.reply_delivery == Some(DeliveryState::Pending)
        && matches!(
            authoritative.state,
            ClaimState::ReplyEnqueued | ClaimState::ReplyConsumed
        )
        && authoritative.request_delivery == stale.request_delivery
        && authoritative.reply == stale.reply
        && authoritative.reply_sha256 == stale.reply_sha256
}

/// One parsed mailbox row. Typed envelopes own their validated message while
/// legacy rows borrow their exact bytes from the single raw mailbox snapshot.
enum MailboxRow<'a> {
    Typed {
        line: &'a str,
        message: MessageV2,
        deliver: bool,
    },
    Legacy {
        line: &'a str,
        exact: &'a str,
    },
}

#[derive(Clone)]
pub struct ProtocolStore {
    root: PathBuf,
    failpoint: Option<ProtocolFailpoint>,
}

impl ProtocolStore {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            failpoint: None,
        }
    }

    fn claim_directory(state: ClaimState) -> ClaimDirectory {
        match state {
            ClaimState::RequestPending | ClaimState::ReplyPending => ClaimDirectory::Pending,
            ClaimState::Open => ClaimDirectory::Open,
            ClaimState::ReplyEnqueued | ClaimState::ReplyConsumed => ClaimDirectory::Terminal,
        }
    }

    /// Update only the exact typed envelopes named by a durable hold manifest.
    /// The caller owns the store lock; legacy rows have no protocol claim.
    pub(crate) fn hold_claim_update_locked(
        &self,
        rows: &[JsonValue],
        consumed: bool,
    ) -> Result<(), String> {
        for row in rows {
            let values = row
                .get::<HashMap<String, JsonValue>>()
                .ok_or("hold manifest row must be an object")?;
            let Some(kind) = values.get("kind").and_then(JsonValue::get::<String>) else {
                continue;
            };
            let kind = MessageKind::parse(kind)?;
            let field = |key: &str| -> Result<&str, String> {
                values
                    .get(key)
                    .and_then(JsonValue::get::<String>)
                    .map(String::as_str)
                    .ok_or_else(|| format!("hold row missing {key}"))
            };
            let correlation_id = field("correlation_id")?;
            let (mut claim, stale) = self
                .resolve_claim_locked(correlation_id)
                .map_err(|error| error.to_string())?
                .ok_or("held typed row has no claim")?;
            // An interrupted claim move leaves a stale predecessor whose
            // recovery predicate requires equal request delivery. Remove it
            // before this update changes the authoritative claim; the reply
            // state stays as recorded.
            if let Some(stale) = stale {
                fs::remove_file(&stale).map_err(|error| error.to_string())?;
            }
            let message = match kind {
                MessageKind::Request => &claim.request,
                MessageKind::TerminalReply => claim
                    .reply
                    .as_ref()
                    .ok_or("held reply has no claim reply")?,
            };
            if message.kind != kind
                || message.id != field("id")?
                || message.correlation_id != correlation_id
                || message.sha256() != field("sha256")?
            {
                return Err("held typed row claim binding mismatch".into());
            }
            let delivery = if consumed {
                DeliveryState::Consumed
            } else {
                DeliveryState::Enqueued
            };
            match kind {
                MessageKind::Request => {
                    if claim.request_delivery == DeliveryState::NotApplicable {
                        continue;
                    }
                    if !matches!(
                        claim.request_delivery,
                        DeliveryState::Enqueued | DeliveryState::Consumed
                    ) {
                        return Err("held request is not enqueued".into());
                    }
                    if claim.request_delivery == delivery {
                        continue;
                    }
                    claim.request_delivery = delivery;
                }
                MessageKind::TerminalReply => {
                    if !matches!(
                        claim.state,
                        ClaimState::ReplyEnqueued | ClaimState::ReplyConsumed
                    ) {
                        return Err("held reply is not enqueued".into());
                    }
                    if claim.reply_delivery == Some(delivery) {
                        continue;
                    }
                    claim.state = if consumed {
                        ClaimState::ReplyConsumed
                    } else {
                        ClaimState::ReplyEnqueued
                    };
                    claim.reply_delivery = Some(delivery);
                }
            }
            claim.updated_at = store::iso_now();
            self.write_claim(Self::claim_directory(claim.state), &claim)
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    pub fn with_failpoint(mut self, failpoint: ProtocolFailpoint) -> Self {
        self.failpoint = Some(failpoint);
        self
    }

    fn fault(&self, point: ProtocolFailpoint) -> Result<(), ProtocolError> {
        if self.failpoint == Some(point) {
            Err(ProtocolError::store(format!(
                "protocol failpoint {point:?}"
            )))
        } else {
            Ok(())
        }
    }

    fn locked<T>(
        &self,
        operation: impl FnOnce() -> Result<T, ProtocolError>,
    ) -> Result<T, ProtocolError> {
        let mut result = None;
        store::with_lock_at(&self.root, || {
            result = Some(operation());
            Ok(())
        })
        .map_err(ProtocolError::store)?;
        result.expect("protocol locked operation executed")
    }

    fn ensure_layout(&self) -> Result<(), ProtocolError> {
        let root = self.root.join("protocol-v1");
        fs::create_dir_all(&root).map_err(ProtocolError::store)?;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
            .map_err(ProtocolError::store)?;
        for directory in [
            ClaimDirectory::Pending,
            ClaimDirectory::Open,
            ClaimDirectory::Terminal,
        ] {
            let path = root.join(directory.name());
            fs::create_dir_all(&path).map_err(ProtocolError::store)?;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))
                .map_err(ProtocolError::store)?;
        }
        fs::create_dir_all(self.root.join("mailbox")).map_err(ProtocolError::store)?;
        Ok(())
    }

    fn claim_path(&self, directory: ClaimDirectory, correlation_id: &str) -> PathBuf {
        self.root
            .join("protocol-v1")
            .join(directory.name())
            .join(format!("{}.json", store::sanitize(correlation_id)))
    }

    fn mailbox_path(&self, recipient_id: &str) -> PathBuf {
        self.root
            .join("mailbox")
            .join(format!("{}.jsonl", store::sanitize(recipient_id)))
    }

    fn registered(&self, session_id: &str) -> Result<bool, ProtocolError> {
        let raw =
            fs::read_to_string(self.root.join("registry.json")).map_err(ProtocolError::store)?;
        let value = raw
            .parse::<JsonValue>()
            .map_err(|error| ProtocolError::store(error.to_string()))?;
        Ok(value
            .get::<HashMap<String, JsonValue>>()
            .and_then(|root| root.get("agents"))
            .and_then(|agents| agents.get::<HashMap<String, JsonValue>>())
            .is_some_and(|agents| agents.contains_key(session_id)))
    }

    fn write_claim(
        &self,
        directory: ClaimDirectory,
        claim: &ClaimStatusV1,
    ) -> Result<(), ProtocolError> {
        claim.validate().map_err(ProtocolError::store)?;
        self.ensure_layout()?;
        let path = self.claim_path(directory, &claim.correlation_id);
        let mut bytes = claim.canonical_bytes();
        bytes.push(b'\n');
        let text = String::from_utf8(bytes).expect("canonical protocol JSON is UTF-8");
        store::atomic_write_private(&path, &text).map_err(ProtocolError::store)
    }

    fn move_claim(
        &self,
        from: ClaimDirectory,
        to: ClaimDirectory,
        claim: &ClaimStatusV1,
        before_source_unlink: Option<ProtocolFailpoint>,
    ) -> Result<(), ProtocolError> {
        self.write_claim(to, claim)?;
        if let Some(point) = before_source_unlink {
            self.fault(point)?;
        }
        let from_path = self.claim_path(from, &claim.correlation_id);
        if from_path != self.claim_path(to, &claim.correlation_id) {
            fs::remove_file(from_path).map_err(ProtocolError::store)?;
        }
        Ok(())
    }

    fn read_claim_in(
        &self,
        directory: ClaimDirectory,
        correlation_id: &str,
    ) -> Result<Option<ClaimStatusV1>, ProtocolError> {
        let path = self.claim_path(directory, correlation_id);
        if !path.exists() {
            return Ok(None);
        }
        let claim = read_jcs_file::<ClaimStatusV1>(&path, None).map_err(ProtocolError::store)?;
        let directory_matches = match directory {
            ClaimDirectory::Pending => matches!(
                claim.state,
                ClaimState::RequestPending | ClaimState::ReplyPending
            ),
            ClaimDirectory::Open => claim.state == ClaimState::Open,
            ClaimDirectory::Terminal => matches!(
                claim.state,
                ClaimState::ReplyEnqueued | ClaimState::ReplyConsumed
            ),
        };
        if !directory_matches || claim.correlation_id != correlation_id {
            return Err(ProtocolError::store(
                "claim directory/state binding mismatch",
            ));
        }
        Ok(Some(claim))
    }

    fn read_claim_locked(
        &self,
        correlation_id: &str,
    ) -> Result<Option<ClaimStatusV1>, ProtocolError> {
        Ok(self
            .resolve_claim_locked(correlation_id)?
            .map(|(claim, _)| claim))
    }

    /// The authoritative claim plus the path of its stale predecessor when an
    /// interrupted move left the correlation in two directories.
    fn resolve_claim_locked(
        &self,
        correlation_id: &str,
    ) -> Result<Option<(ClaimStatusV1, Option<PathBuf>)>, ProtocolError> {
        validate_uuid(correlation_id, "correlation id").map_err(ProtocolError::store)?;
        let pending = self.read_claim_in(ClaimDirectory::Pending, correlation_id)?;
        let open = self.read_claim_in(ClaimDirectory::Open, correlation_id)?;
        let terminal = self.read_claim_in(ClaimDirectory::Terminal, correlation_id)?;
        // A crash between move_claim's destination write and source unlink
        // leaves one correlation in two directories. The later state is
        // authoritative exactly when the earlier file is its consistent stale
        // predecessor; recovery removes the stale file, and every other
        // combination stays fail-closed.
        match (pending, open, terminal) {
            (None, None, None) => Ok(None),
            (Some(claim), None, None) | (None, Some(claim), None) | (None, None, Some(claim)) => {
                Ok(Some((claim, None)))
            }
            (Some(pending), Some(open), None) => {
                if stale_request_pending_of_open(&pending, &open) {
                    let stale = self.claim_path(ClaimDirectory::Pending, correlation_id);
                    Ok(Some((open, Some(stale))))
                } else if stale_open_of_reply_pending(&open, &pending) {
                    let stale = self.claim_path(ClaimDirectory::Open, correlation_id);
                    Ok(Some((pending, Some(stale))))
                } else {
                    Err(ProtocolError::store("duplicate protocol claim"))
                }
            }
            (Some(pending), None, Some(terminal))
                if stale_reply_pending_of_terminal(&pending, &terminal) =>
            {
                let stale = self.claim_path(ClaimDirectory::Pending, correlation_id);
                Ok(Some((terminal, Some(stale))))
            }
            _ => Err(ProtocolError::store("duplicate protocol claim")),
        }
    }

    pub fn read_claim(&self, correlation_id: &str) -> Result<Option<ClaimStatusV1>, ProtocolError> {
        self.locked(|| self.read_claim_locked(correlation_id))
    }

    fn append_message(&self, recipient_id: &str, message: &MessageV2) -> Result<(), ProtocolError> {
        message.validate().map_err(ProtocolError::store)?;
        self.ensure_layout()?;
        let path = self.mailbox_path(recipient_id);
        let exact = String::from_utf8(message.canonical_bytes()).expect("canonical JSON is UTF-8");
        let raw = fs::read_to_string(&path).unwrap_or_default();
        for line in raw.lines() {
            let parsed = line.parse::<JsonValue>().ok();
            let same_id = parsed
                .as_ref()
                .and_then(|value| value.get::<HashMap<String, JsonValue>>())
                .and_then(|value| value.get("id"))
                .and_then(|value| value.get::<String>())
                .is_some_and(|id| id == &message.id);
            if same_id {
                if line == exact {
                    return Ok(());
                }
                return Err(ProtocolError::store(
                    "mailbox message id has different bytes",
                ));
            }
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&path)
            .map_err(ProtocolError::store)?;
        file.write_all(exact.as_bytes())
            .map_err(ProtocolError::store)?;
        file.write_all(b"\n").map_err(ProtocolError::store)?;
        file.sync_all().map_err(ProtocolError::store)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(ProtocolError::store)
    }

    pub fn request(
        &self,
        requester_session_id: &str,
        responder_session_id: &str,
        body: &str,
    ) -> Result<MessageV2, ProtocolError> {
        self.locked(|| {
            self.ensure_layout()?;
            self.recover_pending_locked()?;
            if !self.registered(requester_session_id)? || !self.registered(responder_session_id)? {
                return Err(ProtocolError::store("request endpoints must be registered"));
            }
            let now = store::iso_now();
            let message = MessageV2 {
                schema: 2,
                id: store::uuid_v4(),
                created_at: now.clone(),
                from_session_id: requester_session_id.to_string(),
                to_session_id: responder_session_id.to_string(),
                correlation_id: store::uuid_v4(),
                kind: MessageKind::Request,
                reply_to: None,
                terminal_status: None,
                body: body.to_string(),
                result_sha256: None,
            };
            message.validate().map_err(ProtocolError::store)?;
            let mut claim = ClaimStatusV1 {
                schema: 1,
                correlation_id: message.correlation_id.clone(),
                origin: ClaimOrigin::Message,
                state: ClaimState::RequestPending,
                requester_session_id: requester_session_id.to_string(),
                responder_session_id: responder_session_id.to_string(),
                request_sha256: message.sha256(),
                request: message.clone(),
                request_delivery: DeliveryState::Pending,
                reply: None,
                reply_sha256: None,
                reply_delivery: None,
                created_at: now.clone(),
                updated_at: now,
            };
            self.fault(ProtocolFailpoint::RequestBeforePendingWrite)?;
            self.write_claim(ClaimDirectory::Pending, &claim)?;
            self.fault(ProtocolFailpoint::RequestAfterPendingWrite)?;
            self.fault(ProtocolFailpoint::RequestBeforeMailboxAppend)?;
            self.append_message(responder_session_id, &message)?;
            self.fault(ProtocolFailpoint::RequestAfterMailboxAppend)?;
            claim.state = ClaimState::Open;
            claim.request_delivery = DeliveryState::Enqueued;
            claim.updated_at = store::iso_now();
            self.fault(ProtocolFailpoint::RequestBeforeOpenMove)?;
            self.move_claim(
                ClaimDirectory::Pending,
                ClaimDirectory::Open,
                &claim,
                Some(ProtocolFailpoint::RequestOpenMoveBeforeSourceUnlink),
            )?;
            self.fault(ProtocolFailpoint::RequestAfterOpenMove)?;
            Ok(message)
        })
    }

    fn reply_locked(
        &self,
        correlation_id: &str,
        responder_session_id: &str,
        status: TerminalStatus,
        body: &str,
    ) -> Result<ReplyOutcome, ProtocolError> {
        self.recover_pending_locked()?;
        let mut claim = self
            .read_claim_locked(correlation_id)?
            .ok_or(ProtocolError::UnknownCorrelation)?;
        if claim.responder_session_id != responder_session_id {
            return Err(ProtocolError::UnauthorizedResponder);
        }
        if let Some(existing) = &claim.reply {
            if existing.terminal_status == Some(status) && existing.body == body {
                return Ok(ReplyOutcome {
                    disposition: ReplyDisposition::Idempotent,
                    message: existing.clone(),
                });
            }
            return Err(ProtocolError::CorrelationConflict);
        }
        if claim.state != ClaimState::Open {
            return Err(ProtocolError::CorrelationConflict);
        }
        let message = MessageV2 {
            schema: 2,
            id: store::uuid_v4(),
            created_at: store::iso_now(),
            from_session_id: responder_session_id.to_string(),
            to_session_id: claim.requester_session_id.clone(),
            correlation_id: correlation_id.to_string(),
            kind: MessageKind::TerminalReply,
            reply_to: Some(claim.request.id.clone()),
            terminal_status: Some(status),
            body: body.to_string(),
            result_sha256: None,
        };
        message.validate().map_err(ProtocolError::store)?;
        claim.state = ClaimState::ReplyPending;
        claim.reply = Some(message.clone());
        claim.reply_sha256 = Some(message.sha256());
        claim.reply_delivery = Some(DeliveryState::Pending);
        claim.updated_at = store::iso_now();
        self.fault(ProtocolFailpoint::ReplyBeforePendingWrite)?;
        self.move_claim(
            ClaimDirectory::Open,
            ClaimDirectory::Pending,
            &claim,
            Some(ProtocolFailpoint::ReplyPendingMoveBeforeSourceUnlink),
        )?;
        self.fault(ProtocolFailpoint::ReplyAfterPendingWrite)?;
        self.fault(ProtocolFailpoint::ReplyBeforeMailboxAppend)?;
        self.append_message(&claim.requester_session_id, &message)?;
        self.fault(ProtocolFailpoint::ReplyAfterMailboxAppend)?;
        claim.state = ClaimState::ReplyEnqueued;
        claim.reply_delivery = Some(DeliveryState::Enqueued);
        claim.updated_at = store::iso_now();
        self.fault(ProtocolFailpoint::ReplyBeforeEnqueuedMove)?;
        self.move_claim(
            ClaimDirectory::Pending,
            ClaimDirectory::Terminal,
            &claim,
            Some(ProtocolFailpoint::ReplyEnqueuedMoveBeforeSourceUnlink),
        )?;
        self.fault(ProtocolFailpoint::ReplyAfterEnqueuedMove)?;
        Ok(ReplyOutcome {
            disposition: ReplyDisposition::Created,
            message,
        })
    }

    pub fn reply(
        &self,
        correlation_id: &str,
        responder_session_id: &str,
        status: TerminalStatus,
        body: &str,
    ) -> Result<ReplyOutcome, ProtocolError> {
        self.locked(|| self.reply_locked(correlation_id, responder_session_id, status, body))
    }

    fn recover_pending_locked(&self) -> Result<(), ProtocolError> {
        self.ensure_layout()?;
        let directory = self.root.join("protocol-v1/pending");
        let mut paths = fs::read_dir(&directory)
            .map_err(ProtocolError::store)?
            .map(|entry| {
                entry
                    .map(|entry| entry.path())
                    .map_err(ProtocolError::store)
            })
            .collect::<Result<Vec<_>, _>>()?;
        paths.sort();
        for path in paths {
            let claim =
                read_jcs_file::<ClaimStatusV1>(&path, None).map_err(ProtocolError::store)?;
            if path.file_stem().and_then(|stem| stem.to_str()) != Some(&claim.correlation_id) {
                return Err(ProtocolError::store("pending claim filename mismatch"));
            }
            let open = self.read_claim_in(ClaimDirectory::Open, &claim.correlation_id)?;
            let terminal = self.read_claim_in(ClaimDirectory::Terminal, &claim.correlation_id)?;
            match claim.state {
                ClaimState::RequestPending => {
                    if let Some(open) = open {
                        // Interrupted Pending -> Open move: the destination is
                        // already authoritative and its mailbox append happened
                        // before that write, so only the stale source is
                        // removed. Re-appending here could resurrect an
                        // already-consumed delivery.
                        if terminal.is_some() || !stale_request_pending_of_open(&claim, &open) {
                            return Err(ProtocolError::store("duplicate protocol claim"));
                        }
                        fs::remove_file(&path).map_err(ProtocolError::store)?;
                        continue;
                    }
                    if terminal.is_some() {
                        return Err(ProtocolError::store("duplicate protocol claim"));
                    }
                    self.append_message(&claim.responder_session_id, &claim.request)?;
                    let mut next = claim;
                    next.state = ClaimState::Open;
                    next.request_delivery = DeliveryState::Enqueued;
                    next.updated_at = store::iso_now();
                    self.move_claim(ClaimDirectory::Pending, ClaimDirectory::Open, &next, None)?;
                }
                ClaimState::ReplyPending => {
                    if let Some(terminal) = terminal {
                        // Interrupted Pending -> Terminal move: same rule as
                        // the request move above.
                        if open.is_some() || !stale_reply_pending_of_terminal(&claim, &terminal) {
                            return Err(ProtocolError::store("duplicate protocol claim"));
                        }
                        fs::remove_file(&path).map_err(ProtocolError::store)?;
                        continue;
                    }
                    if let Some(open) = open {
                        // Interrupted Open -> Pending move: here the pending
                        // file is the later authoritative claim. Remove the
                        // stale source before replay so a crash later in this
                        // pass cannot leave an open+terminal pair the pending
                        // scan no longer sees.
                        if !stale_open_of_reply_pending(&open, &claim) {
                            return Err(ProtocolError::store("duplicate protocol claim"));
                        }
                        fs::remove_file(
                            self.claim_path(ClaimDirectory::Open, &claim.correlation_id),
                        )
                        .map_err(ProtocolError::store)?;
                    }
                    let reply = claim
                        .reply
                        .as_ref()
                        .ok_or_else(|| ProtocolError::store("pending reply missing envelope"))?
                        .clone();
                    self.append_message(&claim.requester_session_id, &reply)?;
                    let mut next = claim;
                    next.state = ClaimState::ReplyEnqueued;
                    next.reply_delivery = Some(DeliveryState::Enqueued);
                    next.updated_at = store::iso_now();
                    self.move_claim(
                        ClaimDirectory::Pending,
                        ClaimDirectory::Terminal,
                        &next,
                        None,
                    )?;
                }
                _ => {
                    return Err(ProtocolError::store(
                        "non-pending claim in pending directory",
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn recover_pending(&self) -> Result<(), ProtocolError> {
        self.locked(|| self.recover_pending_locked())
    }

    fn consume_message_locked(&self, message: &MessageV2) -> Result<bool, ProtocolError> {
        let Some(mut claim) = self.read_claim_locked(&message.correlation_id)? else {
            return Err(ProtocolError::store("typed mailbox row has no claim"));
        };
        match message.kind {
            MessageKind::Request
                if claim.request == *message
                    && matches!(
                        claim.state,
                        ClaimState::Open
                            | ClaimState::ReplyPending
                            | ClaimState::ReplyEnqueued
                            | ClaimState::ReplyConsumed
                    )
                    && matches!(
                        claim.request_delivery,
                        DeliveryState::Enqueued | DeliveryState::Consumed
                    ) =>
            {
                let deliver = claim.request_delivery != DeliveryState::Consumed;
                if deliver {
                    claim.request_delivery = DeliveryState::Consumed;
                    claim.updated_at = store::iso_now();
                    self.write_claim(Self::claim_directory(claim.state), &claim)?;
                }
                Ok(deliver)
            }
            MessageKind::TerminalReply
                if claim.reply.as_ref() == Some(message)
                    && matches!(
                        claim.state,
                        ClaimState::ReplyEnqueued | ClaimState::ReplyConsumed
                    ) =>
            {
                let deliver = claim.state != ClaimState::ReplyConsumed;
                if deliver {
                    claim.state = ClaimState::ReplyConsumed;
                    claim.reply_delivery = Some(DeliveryState::Consumed);
                    claim.updated_at = store::iso_now();
                    self.write_claim(ClaimDirectory::Terminal, &claim)?;
                }
                Ok(deliver)
            }
            _ => Err(ProtocolError::store("typed mailbox claim binding mismatch")),
        }
    }

    /// Snapshot renderable rows and their exact identities without consuming claims.
    /// The caller holds the store lock and takes custody of the raw mailbox.
    pub(crate) fn hold_renderable_locked(
        &self,
        recipient_id: &str,
    ) -> Result<(Vec<JsonValue>, Vec<JsonValue>), String> {
        store::recover_holds_locked(&self.root)?;
        self.recover_pending_locked()
            .map_err(|error| error.to_string())?;
        let raw = match fs::read_to_string(self.mailbox_path(recipient_id)) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(error.to_string()),
        };
        let rows = self
            .preflight_mailbox_locked(recipient_id, &raw)
            .map_err(|error| error.to_string())?;
        let mut messages = Vec::with_capacity(rows.len());
        let mut manifest = Vec::with_capacity(rows.len());
        for row in rows {
            let (value, identity) = match row {
                MailboxRow::Typed {
                    message,
                    line,
                    deliver: true,
                    ..
                } => {
                    let value = line
                        .parse::<JsonValue>()
                        .map_err(|_| "malformed typed mailbox row")?;
                    let identity = HashMap::from([
                        ("id".into(), JsonValue::String(message.id.clone())),
                        ("sha256".into(), JsonValue::String(message.sha256())),
                        (
                            "kind".into(),
                            JsonValue::String(message.kind.as_str().into()),
                        ),
                        (
                            "correlation_id".into(),
                            JsonValue::String(message.correlation_id),
                        ),
                    ]);
                    (value, identity)
                }
                MailboxRow::Typed { .. } => continue,
                MailboxRow::Legacy { line, .. } => {
                    let Ok(value) = line.parse::<JsonValue>() else {
                        continue;
                    };
                    let id = value
                        .get::<HashMap<String, JsonValue>>()
                        .and_then(|values| values.get("id"))
                        .cloned()
                        .unwrap_or(JsonValue::Null);
                    let identity = HashMap::from([
                        ("id".into(), id),
                        (
                            "sha256".into(),
                            JsonValue::String(sha256::hex_digest(line.as_bytes())),
                        ),
                        ("kind".into(), JsonValue::Null),
                        ("correlation_id".into(), JsonValue::Null),
                    ]);
                    (value, identity)
                }
            };
            messages.push(value);
            manifest.push(JsonValue::Object(identity));
        }
        Ok((messages, manifest))
    }

    pub(crate) fn drain_renderable_locked(
        &self,
        recipient_id: &str,
    ) -> Result<(Vec<JsonValue>, String), ProtocolError> {
        store::recover_holds_locked(&self.root).map_err(ProtocolError::store)?;
        self.recover_pending_locked()?;
        let raw = fs::read_to_string(self.mailbox_path(recipient_id)).unwrap_or_default();
        let rows = self.preflight_mailbox_locked(recipient_id, &raw)?;

        // Rendering is part of preflight: no claim may transition if a typed
        // row cannot be represented after its authority check succeeds.
        let mut messages = Vec::with_capacity(rows.len());
        for row in &rows {
            match row {
                MailboxRow::Typed {
                    line,
                    deliver: true,
                    ..
                } => {
                    let value = line
                        .parse::<JsonValue>()
                        .map_err(|_| ProtocolError::store("malformed typed mailbox row"))?;
                    messages.push(value);
                }
                MailboxRow::Typed { .. } => {}
                MailboxRow::Legacy { line, .. } => {
                    if let Ok(value) = line.parse::<JsonValue>() {
                        messages.push(value);
                    }
                }
            }
        }

        for row in &rows {
            if let MailboxRow::Typed {
                message,
                deliver: true,
                ..
            } = row
            {
                if !self.consume_message_locked(message)? {
                    return Err(ProtocolError::store(
                        "typed mailbox claim changed after preflight",
                    ));
                }
            }
        }
        drop(rows);
        Ok((messages, raw))
    }

    /// Parse and validate a complete mailbox snapshot without writing. Exact
    /// typed duplicates share one logical delivery; conflicting reuse of a
    /// message id fails before any caller can consume the first row.
    fn preflight_mailbox_locked<'a>(
        &self,
        recipient_id: &str,
        raw: &'a str,
    ) -> Result<Vec<MailboxRow<'a>>, ProtocolError> {
        let mut rows: Vec<MailboxRow<'a>> = Vec::new();
        let mut message_ids: HashMap<String, usize> = HashMap::new();

        for exact in raw.split_inclusive('\n') {
            let line = exact.strip_suffix('\n').unwrap_or(exact);
            let message = match parse_jcs(line.as_bytes(), false).and_then(MessageV2::from_jcs) {
                Ok(message) => message,
                Err(_) => {
                    let typed_looking = line.parse::<JsonValue>().ok().is_some_and(|value| {
                        value
                            .get::<HashMap<String, JsonValue>>()
                            .is_some_and(|object| object.contains_key("schema"))
                    });
                    if typed_looking {
                        return Err(ProtocolError::store("malformed typed mailbox row"));
                    }
                    rows.push(MailboxRow::Legacy { line, exact });
                    continue;
                }
            };

            if message.to_session_id != recipient_id {
                return Err(ProtocolError::store(
                    "typed mailbox recipient binding mismatch",
                ));
            }

            let duplicate = if let Some(index) = message_ids.get(&message.id) {
                match &rows[*index] {
                    MailboxRow::Typed {
                        message: existing, ..
                    } if existing == &message => true,
                    MailboxRow::Typed { .. } => {
                        return Err(ProtocolError::store(
                            "mailbox message id has different bytes",
                        ));
                    }
                    MailboxRow::Legacy { .. } => {
                        unreachable!("typed message id index must address a typed row")
                    }
                }
            } else {
                false
            };

            let deliver = !duplicate && self.peek_message_locked(&message)?;
            if !duplicate {
                message_ids.insert(message.id.clone(), rows.len());
            }
            rows.push(MailboxRow::Typed {
                line,
                message,
                deliver,
            });
        }
        Ok(rows)
    }

    pub fn peek_typed(&self, recipient_id: &str) -> Result<Vec<MessageV2>, ProtocolError> {
        self.locked(|| {
            store::recover_holds_locked(&self.root).map_err(ProtocolError::store)?;
            let raw = fs::read_to_string(self.mailbox_path(recipient_id)).unwrap_or_default();
            let rows = self.preflight_mailbox_locked(recipient_id, &raw)?;
            let mut deliverable = Vec::new();
            for row in rows {
                if let MailboxRow::Typed {
                    message,
                    deliver: true,
                    ..
                } = row
                {
                    deliverable.push(message);
                }
            }
            Ok(deliverable)
        })
    }

    /// Non-draining twin of `consume_message_locked`: every typed row must
    /// bind to its authoritative claim before it is surfaced. Rows an
    /// eventual drain would deliver are shown, already-consumed duplicates
    /// are hidden, and unbound or mismatched rows fail closed. Pending states
    /// stay visible: their mailbox append precedes the interrupted state
    /// move, so the row is real and recovery converges the claim around it.
    fn peek_message_locked(&self, message: &MessageV2) -> Result<bool, ProtocolError> {
        let Some(claim) = self.read_claim_locked(&message.correlation_id)? else {
            return Err(ProtocolError::store("typed mailbox row has no claim"));
        };
        match message.kind {
            MessageKind::Request if claim.request == *message => {
                match (claim.state, claim.request_delivery) {
                    (ClaimState::RequestPending, DeliveryState::Pending) => Ok(true),
                    (
                        ClaimState::Open
                        | ClaimState::ReplyPending
                        | ClaimState::ReplyEnqueued
                        | ClaimState::ReplyConsumed,
                        DeliveryState::Enqueued,
                    ) => Ok(true),
                    (
                        ClaimState::Open
                        | ClaimState::ReplyPending
                        | ClaimState::ReplyEnqueued
                        | ClaimState::ReplyConsumed,
                        DeliveryState::Consumed,
                    ) => Ok(false),
                    _ => Err(ProtocolError::store("typed mailbox claim binding mismatch")),
                }
            }
            MessageKind::TerminalReply if claim.reply.as_ref() == Some(message) => {
                match claim.state {
                    ClaimState::ReplyPending | ClaimState::ReplyEnqueued => Ok(true),
                    ClaimState::ReplyConsumed => Ok(false),
                    _ => Err(ProtocolError::store("typed mailbox claim binding mismatch")),
                }
            }
            _ => Err(ProtocolError::store("typed mailbox claim binding mismatch")),
        }
    }

    pub fn drain_typed(&self, recipient_id: &str) -> Result<Vec<MessageV2>, ProtocolError> {
        self.locked(|| {
            store::recover_holds_locked(&self.root).map_err(ProtocolError::store)?;
            self.recover_pending_locked()?;
            let raw = fs::read_to_string(self.mailbox_path(recipient_id)).unwrap_or_default();
            let rows = self.preflight_mailbox_locked(recipient_id, &raw)?;

            let legacy_len = rows
                .iter()
                .map(|row| match row {
                    MailboxRow::Legacy { exact, .. } => exact.len(),
                    MailboxRow::Typed { .. } => 0,
                })
                .sum();
            let mut legacy = String::with_capacity(legacy_len);
            for row in &rows {
                if let MailboxRow::Legacy { exact, .. } = row {
                    legacy.push_str(exact);
                }
            }

            for row in &rows {
                if let MailboxRow::Typed {
                    message,
                    deliver: true,
                    ..
                } = row
                {
                    if !self.consume_message_locked(message)? {
                        return Err(ProtocolError::store(
                            "typed mailbox claim changed after preflight",
                        ));
                    }
                }
            }

            let mut delivered = Vec::new();
            for row in rows {
                if let MailboxRow::Typed {
                    message,
                    deliver: true,
                    ..
                } = row
                {
                    delivered.push(message);
                }
            }

            let path = self.mailbox_path(recipient_id);
            if legacy.is_empty() {
                let _ = fs::remove_file(path);
            } else if legacy.len() != raw.len() {
                store::atomic_write_private(&path, &legacy).map_err(ProtocolError::store)?;
            }
            Ok(delivered)
        })
    }

    /// Restore claim delivery for typed rows a failed pre-inject delivery is
    /// requeueing. `rows` are exactly the rows the interrupted drain
    /// delivered, i.e. the ones it transitioned to Consumed. The caller holds
    /// the store lock and rewrites the raw mailbox bytes after this returns:
    /// restoring Enqueued first means a crash between the two steps leaves an
    /// undelivered claim without its row (no worse than today's
    /// crash-after-drain window), while the reverse order would leave
    /// requeued rows every later drain silently drops as consumed duplicates.
    pub(crate) fn requeue_consumed_locked(&self, rows: &[JsonValue]) -> Result<(), ProtocolError> {
        for row in rows {
            let is_typed = row
                .get::<HashMap<String, JsonValue>>()
                .is_some_and(|object| object.contains_key("schema"));
            if !is_typed {
                continue;
            }
            let message = MessageV2::from_tinyjson(row).map_err(ProtocolError::store)?;
            let Some(mut claim) = self.read_claim_locked(&message.correlation_id)? else {
                return Err(ProtocolError::store("requeued typed row has no claim"));
            };
            match message.kind {
                MessageKind::Request if claim.request == message => {
                    if matches!(
                        claim.state,
                        ClaimState::Open
                            | ClaimState::ReplyPending
                            | ClaimState::ReplyEnqueued
                            | ClaimState::ReplyConsumed
                    ) && claim.request_delivery == DeliveryState::Consumed
                    {
                        claim.request_delivery = DeliveryState::Enqueued;
                        claim.updated_at = store::iso_now();
                        self.write_claim(Self::claim_directory(claim.state), &claim)?;
                    }
                }
                MessageKind::TerminalReply if claim.reply.as_ref() == Some(&message) => {
                    if claim.state == ClaimState::ReplyConsumed {
                        claim.state = ClaimState::ReplyEnqueued;
                        claim.reply_delivery = Some(DeliveryState::Enqueued);
                        claim.updated_at = store::iso_now();
                        self.write_claim(ClaimDirectory::Terminal, &claim)?;
                    }
                }
                _ => {
                    return Err(ProtocolError::store(
                        "requeued typed row claim binding mismatch",
                    ));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod hold_tests {
    use super::*;

    struct Fixture(ProtocolStore);

    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("relay-hold-claims-{}", store::uuid_v4()));
            Self(ProtocolStore::new(root))
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0.root);
        }
    }

    pub(super) fn claim(state: ClaimState) -> ClaimStatusV1 {
        let request = MessageV2 {
            schema: 2,
            id: "10000000-0000-4000-8000-000000000001".into(),
            correlation_id: "20000000-0000-4000-8000-000000000002".into(),
            created_at: "2026-07-25T12:34:56.789Z".into(),
            from_session_id: "30000000-0000-4000-8000-000000000003".into(),
            to_session_id: "40000000-0000-4000-8000-000000000004".into(),
            kind: MessageKind::Request,
            reply_to: None,
            terminal_status: None,
            body: "request".into(),
            result_sha256: None,
        };
        let reply = (state != ClaimState::Open).then(|| MessageV2 {
            id: "50000000-0000-4000-8000-000000000005".into(),
            from_session_id: request.to_session_id.clone(),
            to_session_id: request.from_session_id.clone(),
            kind: MessageKind::TerminalReply,
            reply_to: Some(request.id.clone()),
            terminal_status: Some(TerminalStatus::Completed),
            result_sha256: None,
            body: "reply".into(),
            ..request.clone()
        });
        ClaimStatusV1 {
            schema: 1,
            correlation_id: request.correlation_id.clone(),
            origin: ClaimOrigin::Message,
            state,
            requester_session_id: request.from_session_id.clone(),
            responder_session_id: request.to_session_id.clone(),
            request_sha256: request.sha256(),
            request_delivery: DeliveryState::Enqueued,
            reply_sha256: reply.as_ref().map(MessageV2::sha256),
            reply_delivery: match state {
                ClaimState::Open => None,
                ClaimState::ReplyPending => Some(DeliveryState::Pending),
                ClaimState::ReplyConsumed => Some(DeliveryState::Consumed),
                _ => Some(DeliveryState::Enqueued),
            },
            reply,
            created_at: request.created_at.clone(),
            updated_at: request.created_at.clone(),
            request,
        }
    }

    pub(super) fn row(message: &MessageV2) -> JsonValue {
        JsonValue::Object(HashMap::from([
            ("id".into(), JsonValue::String(message.id.clone())),
            ("sha256".into(), JsonValue::String(message.sha256())),
            (
                "kind".into(),
                JsonValue::String(message.kind.as_str().into()),
            ),
            (
                "correlation_id".into(),
                JsonValue::String(message.correlation_id.clone()),
            ),
        ]))
    }

    #[test]
    fn request_delivery_survives_reply_state_advancement() {
        for state in [
            ClaimState::Open,
            ClaimState::ReplyPending,
            ClaimState::ReplyEnqueued,
            ClaimState::ReplyConsumed,
        ] {
            let fixture = Fixture::new();
            let original = claim(state);
            fixture
                .0
                .write_claim(ProtocolStore::claim_directory(state), &original)
                .unwrap();
            assert!(fixture.0.peek_message_locked(&original.request).unwrap());
            assert!(fixture.0.consume_message_locked(&original.request).unwrap());
            assert!(!fixture.0.peek_message_locked(&original.request).unwrap());
            assert!(!fixture.0.consume_message_locked(&original.request).unwrap());
            let consumed = fixture
                .0
                .read_claim_locked(&original.correlation_id)
                .unwrap()
                .unwrap();
            assert_eq!(consumed.state, state);
            assert_eq!(consumed.reply_delivery, original.reply_delivery);
            assert_eq!(consumed.request_delivery, DeliveryState::Consumed);
            let mut impostor = original.request.clone();
            impostor.body.push_str(" changed");
            assert!(fixture.0.peek_message_locked(&impostor).is_err());
            assert!(fixture.0.consume_message_locked(&impostor).is_err());
            let request_row = String::from_utf8(original.request.canonical_bytes())
                .unwrap()
                .parse::<JsonValue>()
                .unwrap();
            fixture.0.requeue_consumed_locked(&[request_row]).unwrap();
            assert!(fixture.0.peek_message_locked(&original.request).unwrap());
            let restored = fixture
                .0
                .read_claim_locked(&original.correlation_id)
                .unwrap()
                .unwrap();
            assert_eq!(restored.state, state);
            assert_eq!(restored.reply_delivery, original.reply_delivery);
            assert_eq!(restored.request_delivery, DeliveryState::Enqueued);
        }
    }

    #[test]
    fn held_request_ack_removes_a_stale_reply_predecessor_first() {
        // ReplyPendingMoveBeforeSourceUnlink leaves open/Open beside
        // pending/ReplyPending; ReplyEnqueuedMoveBeforeSourceUnlink leaves
        // pending/ReplyPending beside terminal/ReplyEnqueued. Both pairs share
        // request delivery, which the ack changes on the authoritative file.
        for (stale_state, authoritative_state) in [
            (ClaimState::Open, ClaimState::ReplyPending),
            (ClaimState::ReplyPending, ClaimState::ReplyEnqueued),
        ] {
            let fixture = Fixture::new();
            let authoritative = claim(authoritative_state);
            let mut stale = authoritative.clone();
            stale.state = stale_state;
            if stale_state == ClaimState::Open {
                stale.reply = None;
                stale.reply_sha256 = None;
                stale.reply_delivery = None;
            } else {
                stale.reply_delivery = Some(DeliveryState::Pending);
            }
            let stale_directory = ProtocolStore::claim_directory(stale_state);
            fixture.0.write_claim(stale_directory, &stale).unwrap();
            fixture
                .0
                .write_claim(
                    ProtocolStore::claim_directory(authoritative_state),
                    &authoritative,
                )
                .unwrap();
            let rows = [row(&authoritative.request)];
            fixture.0.hold_claim_update_locked(&rows, true).unwrap();
            let updated = fixture
                .0
                .read_claim_locked(&authoritative.correlation_id)
                .unwrap()
                .unwrap();
            assert_eq!(updated.state, authoritative_state);
            assert_eq!(updated.request_delivery, DeliveryState::Consumed);
            assert_eq!(updated.reply, authoritative.reply);
            assert_eq!(updated.reply_delivery, authoritative.reply_delivery);
            assert!(
                !fixture
                    .0
                    .claim_path(stale_directory, &authoritative.correlation_id)
                    .exists()
            );
            fixture.0.recover_pending_locked().unwrap();
            fixture.0.hold_claim_update_locked(&rows, false).unwrap();
            let restored = fixture
                .0
                .read_claim_locked(&authoritative.correlation_id)
                .unwrap()
                .unwrap();
            assert_eq!(restored.request_delivery, DeliveryState::Enqueued);
        }
    }

    #[test]
    fn held_request_updates_do_not_consume_or_restore_a_later_reply() {
        for state in [
            ClaimState::ReplyPending,
            ClaimState::ReplyEnqueued,
            ClaimState::ReplyConsumed,
        ] {
            let fixture = Fixture::new();
            let original = claim(state);
            fixture
                .0
                .write_claim(ProtocolStore::claim_directory(state), &original)
                .unwrap();
            let rows = [row(&original.request)];
            for consumed in [true, true, false, false] {
                fixture.0.hold_claim_update_locked(&rows, consumed).unwrap();
                let updated = fixture
                    .0
                    .read_claim_locked(&original.correlation_id)
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    updated.request_delivery,
                    if consumed {
                        DeliveryState::Consumed
                    } else {
                        DeliveryState::Enqueued
                    }
                );
                assert_eq!(updated.state, original.state);
                assert_eq!(updated.reply, original.reply);
                assert_eq!(updated.reply_delivery, original.reply_delivery);
            }
        }
    }

    #[test]
    fn held_reply_updates_are_exact_and_preserve_request_delivery() {
        let fixture = Fixture::new();
        let original = claim(ClaimState::ReplyEnqueued);
        fixture
            .0
            .write_claim(ClaimDirectory::Terminal, &original)
            .unwrap();
        let reply = original.reply.as_ref().unwrap();
        for field in ["id", "sha256", "kind", "correlation_id"] {
            let mut invalid = row(reply);
            let values = invalid.get_mut::<HashMap<String, JsonValue>>().unwrap();
            let replacement = match field {
                "sha256" => "b".repeat(64),
                "kind" => MessageKind::Request.as_str().into(),
                _ => "60000000-0000-4000-8000-000000000006".into(),
            };
            values.insert(field.into(), JsonValue::String(replacement));
            assert!(
                fixture
                    .0
                    .hold_claim_update_locked(&[invalid], true)
                    .is_err()
            );
            assert_eq!(
                fixture
                    .0
                    .read_claim_locked(&original.correlation_id)
                    .unwrap()
                    .unwrap(),
                original
            );
        }
        let rows = [row(reply)];
        for consumed in [true, true, false, false] {
            fixture.0.hold_claim_update_locked(&rows, consumed).unwrap();
            let updated = fixture
                .0
                .read_claim_locked(&original.correlation_id)
                .unwrap()
                .unwrap();
            assert_eq!(
                updated.state,
                if consumed {
                    ClaimState::ReplyConsumed
                } else {
                    ClaimState::ReplyEnqueued
                }
            );
            assert_eq!(
                updated.reply_delivery,
                Some(if consumed {
                    DeliveryState::Consumed
                } else {
                    DeliveryState::Enqueued
                })
            );
            assert_eq!(updated.request_delivery, original.request_delivery);
        }
    }
}

#[cfg(test)]
mod hold_recovery_tests {
    use super::*;

    fn interrupted_settlement(consumed: bool) {
        let root = std::env::temp_dir().join(format!("relay-hold-recovery-{}", store::uuid_v4()));
        let protocol = ProtocolStore::new(root.clone());
        let mut claims = Vec::new();
        for _ in 0..2 {
            let mut claim = super::hold_tests::claim(ClaimState::ReplyEnqueued);
            claim.correlation_id = store::uuid_v4();
            claim.request.correlation_id = claim.correlation_id.clone();
            claim.request.id = store::uuid_v4();
            claim.request_sha256 = claim.request.sha256();
            let reply = claim.reply.as_mut().unwrap();
            reply.correlation_id = claim.correlation_id.clone();
            reply.id = store::uuid_v4();
            reply.reply_to = Some(claim.request.id.clone());
            claim.reply_sha256 = Some(reply.sha256());
            protocol
                .write_claim(ClaimDirectory::Terminal, &claim)
                .unwrap();
            claims.push(claim);
        }
        let recipient = &claims[0].requester_session_id;
        let raw: String = claims.iter().fold(String::new(), |mut raw, claim| {
            raw.push_str(
                &String::from_utf8(claim.reply.as_ref().unwrap().canonical_bytes()).unwrap(),
            );
            raw.push('\n');
            raw
        });
        fs::create_dir_all(root.join("mailbox")).unwrap();
        fs::write(protocol.mailbox_path(recipient), &raw).unwrap();
        let holds = store::HoldStore::new(root.clone());
        let mut receipt = None;
        store::with_lock_at(&root, || {
            receipt = Some(holds.hold_locked(recipient, "session_start", 60)?);
            Ok(())
        })
        .unwrap();
        let token = receipt.unwrap().token.unwrap();
        if !consumed {
            // An interrupted prior claim update must be reversed by restoring.
            for claim in &claims {
                protocol
                    .hold_claim_update_locked(
                        &[super::hold_tests::row(claim.reply.as_ref().unwrap())],
                        true,
                    )
                    .unwrap();
            }
        }
        let failing = store::HoldStore::new(root.clone())
            .with_failpoint(store::HoldFailpoint::AfterClaimUpdate(0));
        assert!(
            if consumed {
                failing.ack_hold(&token)
            } else {
                failing.rollback_hold(&token)
            }
            .is_err()
        );
        let first = protocol
            .read_claim_locked(&claims[0].correlation_id)
            .unwrap()
            .unwrap();
        let second = protocol
            .read_claim_locked(&claims[1].correlation_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            first.state,
            if consumed {
                ClaimState::ReplyConsumed
            } else {
                ClaimState::ReplyEnqueued
            }
        );
        assert_eq!(
            second.state,
            if consumed {
                ClaimState::ReplyEnqueued
            } else {
                ClaimState::ReplyConsumed
            }
        );
        store::with_lock_at(&root, || holds.recover_locked()).unwrap();
        for original in &claims {
            let recovered = protocol
                .read_claim_locked(&original.correlation_id)
                .unwrap()
                .unwrap();
            assert_eq!(
                recovered.state,
                if consumed {
                    ClaimState::ReplyConsumed
                } else {
                    ClaimState::ReplyEnqueued
                }
            );
            assert_eq!(recovered.request_delivery, original.request_delivery);
        }
        if consumed {
            assert!(!protocol.mailbox_path(recipient).exists());
        } else {
            assert_eq!(
                fs::read_to_string(protocol.mailbox_path(recipient)).unwrap(),
                raw
            );
            assert_eq!(
                protocol.peek_typed(recipient).unwrap(),
                claims
                    .iter()
                    .map(|claim| claim.reply.clone().unwrap())
                    .collect::<Vec<_>>()
            );
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn interrupted_typed_commit_finishes_remaining_rows() {
        interrupted_settlement(true);
    }

    #[test]
    fn interrupted_typed_restore_finishes_remaining_rows_without_duplicate_payload() {
        interrupted_settlement(false);
    }

    #[test]
    fn expired_typed_hold_is_restored_by_peek_without_consuming() {
        let root = std::env::temp_dir().join(format!("relay-hold-expiry-{}", store::uuid_v4()));
        let protocol = ProtocolStore::new(root.clone());
        let original = super::hold_tests::claim(ClaimState::Open);
        protocol
            .write_claim(ClaimDirectory::Open, &original)
            .unwrap();
        protocol
            .append_message(&original.responder_session_id, &original.request)
            .unwrap();
        let holds = store::HoldStore::new(root.clone());
        store::with_lock_at(&root, || {
            let receipt = holds.hold_locked(&original.responder_session_id, "session_start", 0)?;
            assert_eq!(receipt.count, 1);
            assert!(receipt.token.is_some());
            Ok(())
        })
        .unwrap();
        assert!(
            !protocol
                .mailbox_path(&original.responder_session_id)
                .exists()
        );
        assert_eq!(
            protocol.peek_typed(&original.responder_session_id).unwrap(),
            vec![original.request.clone()]
        );
        assert_eq!(
            protocol.peek_typed(&original.responder_session_id).unwrap(),
            vec![original.request.clone()]
        );
        let restored = protocol
            .read_claim(&original.correlation_id)
            .unwrap()
            .unwrap();
        assert_eq!(restored.request_delivery, DeliveryState::Enqueued);
        assert_eq!(
            protocol
                .drain_typed(&original.responder_session_id)
                .unwrap(),
            vec![original.request]
        );
        assert!(
            protocol
                .drain_typed(&original.responder_session_id)
                .unwrap()
                .is_empty()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reply_to_held_request_delivers_once_and_request_ack_preserves_it() {
        let root = std::env::temp_dir().join(format!("relay-hold-reply-{}", store::uuid_v4()));
        let protocol = ProtocolStore::new(root.clone());
        let original = super::hold_tests::claim(ClaimState::Open);
        protocol
            .write_claim(ClaimDirectory::Open, &original)
            .unwrap();
        protocol
            .append_message(&original.responder_session_id, &original.request)
            .unwrap();
        let holds = store::HoldStore::new(root.clone());
        let mut receipt = None;
        store::with_lock_at(&root, || {
            receipt =
                Some(holds.hold_locked(&original.responder_session_id, "session_start", 60)?);
            Ok(())
        })
        .unwrap();
        let token = receipt.unwrap().token.unwrap();
        let reply = protocol
            .reply(
                &original.correlation_id,
                &original.responder_session_id,
                TerminalStatus::Completed,
                "completed during hold",
            )
            .unwrap()
            .message;
        assert!(
            protocol
                .peek_typed(&original.responder_session_id)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            protocol.peek_typed(&original.requester_session_id).unwrap(),
            vec![reply.clone()]
        );
        assert_eq!(
            protocol
                .drain_typed(&original.requester_session_id)
                .unwrap(),
            vec![reply.clone()]
        );
        assert!(
            protocol
                .drain_typed(&original.requester_session_id)
                .unwrap()
                .is_empty()
        );
        let before_ack = protocol
            .read_claim(&original.correlation_id)
            .unwrap()
            .unwrap();
        assert_eq!(before_ack.request_delivery, DeliveryState::Enqueued);
        assert_eq!(before_ack.state, ClaimState::ReplyConsumed);
        holds.ack_hold(&token).unwrap();
        let after_ack = protocol
            .read_claim(&original.correlation_id)
            .unwrap()
            .unwrap();
        assert_eq!(after_ack.request_delivery, DeliveryState::Consumed);
        assert_eq!(after_ack.state, ClaimState::ReplyConsumed);
        assert_eq!(after_ack.reply_delivery, Some(DeliveryState::Consumed));
        assert_eq!(after_ack.reply, Some(reply));
        assert!(
            protocol
                .drain_typed(&original.requester_session_id)
                .unwrap()
                .is_empty()
        );
        assert!(
            protocol
                .drain_typed(&original.responder_session_id)
                .unwrap()
                .is_empty()
        );
        fs::remove_dir_all(root).unwrap();
    }
}
