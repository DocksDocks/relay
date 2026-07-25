use crate::sha256;
use crate::store;
pub use crate::workspace::schema::ObjectFormat;
use crate::workspace::schema::{
    AbsPath, ClosedJcs, Decimal, JcsValue, LowerUuidV4, RelPath, Sha256Digest, parse_jcs,
    read_jcs_file, serialize_jcs,
};
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
    WorkerResult,
}

impl MessageKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Request => "request",
            Self::TerminalReply => "terminal_reply",
            Self::WorkerResult => "worker_result",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "request" => Ok(Self::Request),
            "terminal_reply" => Ok(Self::TerminalReply),
            "worker_result" => Ok(Self::WorkerResult),
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
            ("from session id", self.from_session_id.as_str()),
            ("to session id", self.to_session_id.as_str()),
        ] {
            validate_uuid(value, label)?;
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
            MessageKind::WorkerResult => {
                self.reply_to.is_some()
                    && self.terminal_status.is_some()
                    && self.result_sha256.is_some()
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
    Fanout,
}

impl ClaimOrigin {
    fn as_str(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Fanout => "fanout",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "message" => Ok(Self::Message),
            "fanout" => Ok(Self::Fanout),
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
        for (label, value) in [
            ("claim correlation id", self.correlation_id.as_str()),
            ("claim requester", self.requester_session_id.as_str()),
            ("claim responder", self.responder_session_id.as_str()),
        ] {
            validate_uuid(value, label)?;
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
        let legal_request_delivery = match self.origin {
            ClaimOrigin::Message => matches!(
                self.request_delivery,
                DeliveryState::Enqueued | DeliveryState::Consumed
            ),
            ClaimOrigin::Fanout => self.request_delivery == DeliveryState::NotApplicable,
        };
        let legal = match (self.origin, self.state) {
            (ClaimOrigin::Message, ClaimState::RequestPending) => {
                self.request_delivery == DeliveryState::Pending
                    && !has_reply
                    && self.reply_delivery.is_none()
            }
            (_, ClaimState::Open) => {
                legal_request_delivery && !has_reply && self.reply_delivery.is_none()
            }
            (_, ClaimState::ReplyPending) => {
                legal_request_delivery
                    && has_reply
                    && self.reply_delivery == Some(DeliveryState::Pending)
            }
            (_, ClaimState::ReplyEnqueued) => {
                legal_request_delivery
                    && has_reply
                    && self.reply_delivery == Some(DeliveryState::Enqueued)
            }
            (_, ClaimState::ReplyConsumed) => {
                legal_request_delivery
                    && has_reply
                    && self.reply_delivery == Some(DeliveryState::Consumed)
            }
            _ => false,
        };
        if !legal {
            return Err("claim origin/state/delivery matrix violation".to_string());
        }
        match (&self.reply, &self.reply_sha256, self.origin) {
            (None, None, _) => {}
            (Some(reply), Some(reply_sha256), origin) => {
                reply.validate()?;
                let expected_kind = match origin {
                    ClaimOrigin::Message => MessageKind::TerminalReply,
                    ClaimOrigin::Fanout => MessageKind::WorkerResult,
                };
                if reply.kind != expected_kind
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerResultV1 {
    pub schema: u8,
    pub result_id: String,
    pub correlation_id: String,
    pub reservation_id: String,
    pub root_reservation_id: String,
    pub parent_session_id: String,
    pub worker_id: String,
    pub generation: String,
    pub runtime_session_id: String,
    pub repo_common_dir: String,
    pub repo_dev: String,
    pub repo_ino: String,
    pub object_format: ObjectFormat,
    pub base_commit: String,
    pub handback_commit: String,
    pub status: TerminalStatus,
    pub summary: String,
    pub changed_paths: Vec<String>,
    pub created_at: String,
}

impl WorkerResultV1 {
    pub fn canonical_bytes(&self) -> Vec<u8> {
        canonical_bytes(self)
    }

    pub fn sha256(&self) -> String {
        digest(self)
    }

    fn validate(&self) -> Result<(), String> {
        if self.schema != 1 {
            return Err("WorkerResultV1 schema mismatch".to_string());
        }
        for (label, value) in [
            ("result id", self.result_id.as_str()),
            ("result correlation", self.correlation_id.as_str()),
            ("reservation id", self.reservation_id.as_str()),
            ("root reservation id", self.root_reservation_id.as_str()),
            ("parent session id", self.parent_session_id.as_str()),
            ("worker id", self.worker_id.as_str()),
            ("generation", self.generation.as_str()),
            ("runtime session id", self.runtime_session_id.as_str()),
        ] {
            validate_uuid(value, label)?;
        }
        validate_timestamp(&self.created_at, "worker result created_at")?;
        for (label, value) in [("repo_dev", &self.repo_dev), ("repo_ino", &self.repo_ino)] {
            Decimal::parse(value)
                .and_then(|_| {
                    value
                        .parse::<u64>()
                        .map(|_| ())
                        .map_err(|_| "overflow".into())
                })
                .map_err(|_| format!("{label} is not an in-range canonical u64"))?;
        }
        if self.repo_common_dir.contains("//")
            || self.repo_common_dir.contains("/./")
            || self.repo_common_dir.contains("/../")
            || self.repo_common_dir.ends_with("/.")
            || self.repo_common_dir.ends_with("/..")
            || self.repo_common_dir.ends_with('/')
            || AbsPath::parse(&self.repo_common_dir).is_err()
        {
            return Err("repo_common_dir is not a lexical canonical absolute path".to_string());
        }
        for oid in [&self.base_commit, &self.handback_commit] {
            if oid.len() != self.object_format.oid_len()
                || !oid
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
            {
                return Err("worker result Git OID is invalid".to_string());
            }
        }
        if self.summary.len() > 4096 || self.summary.contains('\0') {
            return Err("worker result summary is outside UTF-8 byte bounds".to_string());
        }
        if self.changed_paths.len() > 4096 {
            return Err("worker result has too many changed paths".to_string());
        }
        for path in &self.changed_paths {
            RelPath::parse(path)?;
        }
        if self.changed_paths.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err("worker result changed paths are not sorted and unique".to_string());
        }
        if serialize_jcs(&self.to_jcs()).len() > 1024 * 1024 {
            return Err("WorkerResultV1 exceeds one MiB".to_string());
        }
        Ok(())
    }
}

impl ClosedJcs for WorkerResultV1 {
    fn from_jcs(value: JcsValue) -> Result<Self, String> {
        let values = value.object()?;
        let expected = [
            "schema",
            "result_id",
            "correlation_id",
            "reservation_id",
            "root_reservation_id",
            "parent_session_id",
            "worker_id",
            "generation",
            "runtime_session_id",
            "repo_common_dir",
            "repo_dev",
            "repo_ino",
            "object_format",
            "base_commit",
            "handback_commit",
            "status",
            "summary",
            "changed_paths",
            "created_at",
        ];
        require_keys(&values, &expected)?;
        let changed_paths = match values
            .get("changed_paths")
            .ok_or_else(|| "missing changed_paths".to_string())?
        {
            JcsValue::Array(paths) => paths
                .iter()
                .map(|path| path.as_str().map(str::to_string))
                .collect::<Result<Vec<_>, _>>()?,
            _ => return Err("changed_paths must be an array".to_string()),
        };
        let result = Self {
            schema: schema(&values, 1)?,
            result_id: string(&values, "result_id")?,
            correlation_id: string(&values, "correlation_id")?,
            reservation_id: string(&values, "reservation_id")?,
            root_reservation_id: string(&values, "root_reservation_id")?,
            parent_session_id: string(&values, "parent_session_id")?,
            worker_id: string(&values, "worker_id")?,
            generation: string(&values, "generation")?,
            runtime_session_id: string(&values, "runtime_session_id")?,
            repo_common_dir: string(&values, "repo_common_dir")?,
            repo_dev: string(&values, "repo_dev")?,
            repo_ino: string(&values, "repo_ino")?,
            object_format: ObjectFormat::parse(&string(&values, "object_format")?)?,
            base_commit: string(&values, "base_commit")?,
            handback_commit: string(&values, "handback_commit")?,
            status: TerminalStatus::parse(&string(&values, "status")?)?,
            summary: string(&values, "summary")?,
            changed_paths,
            created_at: string(&values, "created_at")?,
        };
        result.validate()?;
        Ok(result)
    }

    fn to_jcs(&self) -> JcsValue {
        object([
            ("base_commit", JcsValue::String(self.base_commit.clone())),
            (
                "changed_paths",
                JcsValue::Array(
                    self.changed_paths
                        .iter()
                        .cloned()
                        .map(JcsValue::String)
                        .collect(),
                ),
            ),
            (
                "correlation_id",
                JcsValue::String(self.correlation_id.clone()),
            ),
            ("created_at", JcsValue::String(self.created_at.clone())),
            ("generation", JcsValue::String(self.generation.clone())),
            (
                "handback_commit",
                JcsValue::String(self.handback_commit.clone()),
            ),
            (
                "object_format",
                JcsValue::String(self.object_format.as_str().into()),
            ),
            (
                "parent_session_id",
                JcsValue::String(self.parent_session_id.clone()),
            ),
            (
                "repo_common_dir",
                JcsValue::String(self.repo_common_dir.clone()),
            ),
            ("repo_dev", JcsValue::String(self.repo_dev.clone())),
            ("repo_ino", JcsValue::String(self.repo_ino.clone())),
            (
                "reservation_id",
                JcsValue::String(self.reservation_id.clone()),
            ),
            ("result_id", JcsValue::String(self.result_id.clone())),
            (
                "root_reservation_id",
                JcsValue::String(self.root_reservation_id.clone()),
            ),
            (
                "runtime_session_id",
                JcsValue::String(self.runtime_session_id.clone()),
            ),
            ("schema", JcsValue::Integer(i64::from(self.schema))),
            ("status", JcsValue::String(self.status.as_str().into())),
            ("summary", JcsValue::String(self.summary.clone())),
            ("worker_id", JcsValue::String(self.worker_id.clone())),
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
    RequestAfterOpenMove,
    ReplyBeforePendingWrite,
    ReplyAfterPendingWrite,
    ReplyBeforeMailboxAppend,
    ReplyAfterMailboxAppend,
    ReplyBeforeEnqueuedMove,
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
    ) -> Result<(), ProtocolError> {
        self.write_claim(to, claim)?;
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
        validate_uuid(correlation_id, "correlation id").map_err(ProtocolError::store)?;
        let mut found = None;
        for directory in [
            ClaimDirectory::Pending,
            ClaimDirectory::Open,
            ClaimDirectory::Terminal,
        ] {
            if let Some(claim) = self.read_claim_in(directory, correlation_id)? {
                if found.is_some() {
                    return Err(ProtocolError::store("duplicate protocol claim"));
                }
                found = Some(claim);
            }
        }
        Ok(found)
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
            self.move_claim(ClaimDirectory::Pending, ClaimDirectory::Open, &claim)?;
            self.fault(ProtocolFailpoint::RequestAfterOpenMove)?;
            Ok(message)
        })
    }

    pub fn open_fanout_claim(
        &self,
        correlation_id: &str,
        requester_session_id: &str,
        responder_session_id: &str,
        body: &str,
    ) -> Result<MessageV2, ProtocolError> {
        self.locked(|| {
            self.ensure_layout()?;
            self.recover_pending_locked()?;
            validate_uuid(correlation_id, "fanout correlation").map_err(ProtocolError::store)?;
            if !self.registered(requester_session_id)? || !self.registered(responder_session_id)? {
                return Err(ProtocolError::store(
                    "fanout claim endpoints must be registered",
                ));
            }
            if let Some(existing) = self.read_claim_locked(correlation_id)? {
                if existing.origin == ClaimOrigin::Fanout
                    && existing.state == ClaimState::Open
                    && existing.requester_session_id == requester_session_id
                    && existing.responder_session_id == responder_session_id
                    && existing.request.body == body
                {
                    return Ok(existing.request);
                }
                return Err(ProtocolError::CorrelationConflict);
            }
            let now = store::iso_now();
            let message = MessageV2 {
                schema: 2,
                id: store::uuid_v4(),
                created_at: now.clone(),
                from_session_id: requester_session_id.to_string(),
                to_session_id: responder_session_id.to_string(),
                correlation_id: correlation_id.to_string(),
                kind: MessageKind::Request,
                reply_to: None,
                terminal_status: None,
                body: body.to_string(),
                result_sha256: None,
            };
            message.validate().map_err(ProtocolError::store)?;
            let claim = ClaimStatusV1 {
                schema: 1,
                correlation_id: correlation_id.to_string(),
                origin: ClaimOrigin::Fanout,
                state: ClaimState::Open,
                requester_session_id: requester_session_id.to_string(),
                responder_session_id: responder_session_id.to_string(),
                request_sha256: message.sha256(),
                request: message.clone(),
                request_delivery: DeliveryState::NotApplicable,
                reply: None,
                reply_sha256: None,
                reply_delivery: None,
                created_at: now.clone(),
                updated_at: now,
            };
            self.write_claim(ClaimDirectory::Open, &claim)?;
            Ok(message)
        })
    }

    fn reply_locked(
        &self,
        correlation_id: &str,
        responder_session_id: &str,
        status: TerminalStatus,
        body: &str,
        kind: MessageKind,
        result_sha256: Option<String>,
    ) -> Result<ReplyOutcome, ProtocolError> {
        self.recover_pending_locked()?;
        let mut claim = self
            .read_claim_locked(correlation_id)?
            .ok_or(ProtocolError::UnknownCorrelation)?;
        if claim.responder_session_id != responder_session_id {
            return Err(ProtocolError::UnauthorizedResponder);
        }
        if let Some(existing) = &claim.reply {
            if existing.kind == kind
                && existing.terminal_status == Some(status)
                && existing.body == body
                && existing.result_sha256 == result_sha256
            {
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
            kind,
            reply_to: Some(claim.request.id.clone()),
            terminal_status: Some(status),
            body: body.to_string(),
            result_sha256,
        };
        message.validate().map_err(ProtocolError::store)?;
        claim.state = ClaimState::ReplyPending;
        claim.reply = Some(message.clone());
        claim.reply_sha256 = Some(message.sha256());
        claim.reply_delivery = Some(DeliveryState::Pending);
        claim.updated_at = store::iso_now();
        self.fault(ProtocolFailpoint::ReplyBeforePendingWrite)?;
        self.move_claim(ClaimDirectory::Open, ClaimDirectory::Pending, &claim)?;
        self.fault(ProtocolFailpoint::ReplyAfterPendingWrite)?;
        self.fault(ProtocolFailpoint::ReplyBeforeMailboxAppend)?;
        self.append_message(&claim.requester_session_id, &message)?;
        self.fault(ProtocolFailpoint::ReplyAfterMailboxAppend)?;
        claim.state = ClaimState::ReplyEnqueued;
        claim.reply_delivery = Some(DeliveryState::Enqueued);
        claim.updated_at = store::iso_now();
        self.fault(ProtocolFailpoint::ReplyBeforeEnqueuedMove)?;
        self.move_claim(ClaimDirectory::Pending, ClaimDirectory::Terminal, &claim)?;
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
        self.locked(|| {
            self.reply_locked(
                correlation_id,
                responder_session_id,
                status,
                body,
                MessageKind::TerminalReply,
                None,
            )
        })
    }

    pub fn publish_worker_result(
        &self,
        result: &WorkerResultV1,
        body: &str,
    ) -> Result<ReplyOutcome, ProtocolError> {
        result.validate().map_err(ProtocolError::store)?;
        self.locked(|| {
            let claim = self
                .read_claim_locked(&result.correlation_id)?
                .ok_or(ProtocolError::UnknownCorrelation)?;
            if claim.origin != ClaimOrigin::Fanout
                || claim.responder_session_id != result.runtime_session_id
                || claim.requester_session_id != result.parent_session_id
            {
                return Err(ProtocolError::CorrelationConflict);
            }
            self.reply_locked(
                &result.correlation_id,
                &result.runtime_session_id,
                result.status,
                body,
                MessageKind::WorkerResult,
                Some(result.sha256()),
            )
        })
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
            match claim.state {
                ClaimState::RequestPending => {
                    self.append_message(&claim.responder_session_id, &claim.request)?;
                    let mut next = claim;
                    next.state = ClaimState::Open;
                    next.request_delivery = DeliveryState::Enqueued;
                    next.updated_at = store::iso_now();
                    self.move_claim(ClaimDirectory::Pending, ClaimDirectory::Open, &next)?;
                }
                ClaimState::ReplyPending => {
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
                    self.move_claim(ClaimDirectory::Pending, ClaimDirectory::Terminal, &next)?;
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

    fn consume_message_locked(
        &self,
        message: &MessageV2,
        claim_required: bool,
    ) -> Result<bool, ProtocolError> {
        let Some(mut claim) = self.read_claim_locked(&message.correlation_id)? else {
            return if claim_required {
                Err(ProtocolError::store("typed mailbox row has no claim"))
            } else {
                Ok(true)
            };
        };
        match message.kind {
            MessageKind::Request
                if claim.request == *message
                    && claim.state == ClaimState::Open
                    && matches!(
                        claim.request_delivery,
                        DeliveryState::Enqueued | DeliveryState::Consumed
                    ) =>
            {
                let deliver = claim.request_delivery != DeliveryState::Consumed;
                if deliver {
                    claim.request_delivery = DeliveryState::Consumed;
                    claim.updated_at = store::iso_now();
                    self.write_claim(ClaimDirectory::Open, &claim)?;
                }
                Ok(deliver)
            }
            MessageKind::TerminalReply | MessageKind::WorkerResult
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

    pub(crate) fn drain_renderable_locked(
        &self,
        recipient_id: &str,
    ) -> Result<(Vec<JsonValue>, String), ProtocolError> {
        self.recover_pending_locked()?;
        let raw = fs::read_to_string(self.mailbox_path(recipient_id)).unwrap_or_default();
        let mut messages = Vec::new();
        for line in raw.lines().filter(|line| !line.is_empty()) {
            let Ok(value) = line.parse::<JsonValue>() else {
                continue;
            };
            let is_typed = value
                .get::<HashMap<String, JsonValue>>()
                .is_some_and(|object| object.contains_key("schema"));
            let deliver = if is_typed {
                let message = MessageV2::from_tinyjson(&value).map_err(ProtocolError::store)?;
                if message.to_session_id != recipient_id {
                    return Err(ProtocolError::store(
                        "typed mailbox recipient binding mismatch",
                    ));
                }
                self.consume_message_locked(&message, false)?
            } else {
                true
            };
            if deliver {
                messages.push(value);
            }
        }
        Ok((messages, raw))
    }

    fn typed_mailbox(
        &self,
        recipient_id: &str,
    ) -> Result<(Vec<MessageV2>, Vec<String>), ProtocolError> {
        let raw = fs::read_to_string(self.mailbox_path(recipient_id)).unwrap_or_default();
        let mut typed = Vec::new();
        let mut legacy = Vec::new();
        for line in raw.lines() {
            match parse_jcs(line.as_bytes(), false).and_then(MessageV2::from_jcs) {
                Ok(message) if message.to_session_id == recipient_id => typed.push(message),
                Ok(_) => {
                    return Err(ProtocolError::store(
                        "typed mailbox recipient binding mismatch",
                    ));
                }
                Err(_) => {
                    let typed_looking = line
                        .parse::<JsonValue>()
                        .ok()
                        .and_then(|value| value.get::<HashMap<String, JsonValue>>().cloned())
                        .is_some_and(|object| object.contains_key("schema"));
                    if typed_looking {
                        return Err(ProtocolError::store("malformed typed mailbox row"));
                    }
                    legacy.push(line.to_string());
                }
            }
        }
        Ok((typed, legacy))
    }

    pub fn peek_typed(&self, recipient_id: &str) -> Result<Vec<MessageV2>, ProtocolError> {
        self.locked(|| self.typed_mailbox(recipient_id).map(|(typed, _)| typed))
    }

    pub fn drain_typed(&self, recipient_id: &str) -> Result<Vec<MessageV2>, ProtocolError> {
        self.locked(|| {
            self.recover_pending_locked()?;
            let (typed, legacy) = self.typed_mailbox(recipient_id)?;
            let mut delivered = Vec::with_capacity(typed.len());
            for message in typed {
                if self.consume_message_locked(&message, true)? {
                    delivered.push(message);
                }
            }
            let path = self.mailbox_path(recipient_id);
            if legacy.is_empty() {
                let _ = fs::remove_file(path);
            } else {
                let mut text = legacy.join("\n");
                text.push('\n');
                store::atomic_write_private(&path, &text).map_err(ProtocolError::store)?;
            }
            Ok(delivered)
        })
    }
}
