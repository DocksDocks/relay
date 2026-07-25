pub mod support;

use relay::lifecycle::{self, Admission, LifecycleStore, OperationKind};
use relay::protocol::{
    ClaimOrigin, ClaimState, ClaimStatusV1, DeliveryState, MessageKind, MessageV2, ObjectFormat,
    ProtocolError, ProtocolFailpoint, ProtocolStore, ReplyDisposition, TerminalStatus,
    WorkerResultV1,
};
use relay::workspace::schema::{ClosedJcs, JcsValue, LowerUuidV4, parse_jcs, serialize_jcs};
use std::collections::HashMap;
use std::fmt::Debug;
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};
use std::thread;
use support::fresh_home;
use tinyjson::JsonValue;

const REQUEST_ID: &str = "10000000-0000-4000-8000-000000000001";
const CORRELATION_ID: &str = "20000000-0000-4000-8000-000000000002";
const REQUESTER_ID: &str = "30000000-0000-4000-8000-000000000003";
const RESPONDER_ID: &str = "40000000-0000-4000-8000-000000000004";
const REPLY_ID: &str = "50000000-0000-4000-8000-000000000005";
const RESULT_ID: &str = "60000000-0000-4000-8000-000000000006";
const RESERVATION_ID: &str = "70000000-0000-4000-8000-000000000007";
const ROOT_RESERVATION_ID: &str = "80000000-0000-4000-8000-000000000008";
const WORKER_ID: &str = "90000000-0000-4000-8000-000000000009";
const GENERATION_ID: &str = "a0000000-0000-4000-8000-00000000000a";
const THIRD_SESSION_ID: &str = "b0000000-0000-4000-8000-00000000000b";
const UNKNOWN_SESSION_ID: &str = "c0000000-0000-4000-8000-00000000000c";
const CREATED_AT: &str = "2026-07-25T12:34:56.789Z";
const UPDATED_AT: &str = "2026-07-25T12:35:00.001Z";
const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SHA_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const REQUEST_SHA256: &str = "5f9b7e13f3a003db70073110d0761fbfea8dccbc5ab2faca0cf875c6e3375607";
const CLAIM_SHA256: &str = "53302c8de9b7f61950c3a05d01f18c931a1b57b90913dd3fe9dfeb7c0b787054";
const WORKER_RESULT_SHA256: &str =
    "55b99b0b80eabdcb794bfa8c72866e543ced5589cde75d15eb9066cca2a3a34b";

const CANONICAL_REQUEST: &str = concat!(
    r#"{"body":"compile exact","correlation_id":"20000000-0000-4000-8000-000000000002","created_at":"2026-07-25T12:34:56.789Z","from_session_id":"30000000-0000-4000-8000-000000000003","id":"10000000-0000-4000-8000-000000000001","kind":"request","reply_to":null,"result_sha256":null,"schema":2,"terminal_status":null,"to_session_id":"40000000-0000-4000-8000-000000000004"}"#,
);
const CANONICAL_OPEN_CLAIM: &str = concat!(
    r#"{"correlation_id":"20000000-0000-4000-8000-000000000002","created_at":"2026-07-25T12:34:56.789Z","origin":"message","reply":null,"reply_delivery":null,"reply_sha256":null,"request":{"body":"compile exact","correlation_id":"20000000-0000-4000-8000-000000000002","created_at":"2026-07-25T12:34:56.789Z","from_session_id":"30000000-0000-4000-8000-000000000003","id":"10000000-0000-4000-8000-000000000001","kind":"request","reply_to":null,"result_sha256":null,"schema":2,"terminal_status":null,"to_session_id":"40000000-0000-4000-8000-000000000004"},"request_delivery":"enqueued","request_sha256":"5f9b7e13f3a003db70073110d0761fbfea8dccbc5ab2faca0cf875c6e3375607","requester_session_id":"30000000-0000-4000-8000-000000000003","responder_session_id":"40000000-0000-4000-8000-000000000004","schema":1,"state":"Open","updated_at":"2026-07-25T12:35:00.001Z"}"#,
);
const CANONICAL_WORKER_RESULT: &str = concat!(
    r#"{"base_commit":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","changed_paths":["src/protocol.rs","tests/protocol.rs"],"correlation_id":"20000000-0000-4000-8000-000000000002","created_at":"2026-07-25T12:34:56.789Z","generation":"a0000000-0000-4000-8000-00000000000a","handback_commit":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","object_format":"sha1","parent_session_id":"30000000-0000-4000-8000-000000000003","repo_common_dir":"/tmp/protocol-repository/.git","repo_dev":"2049","repo_ino":"987654","reservation_id":"70000000-0000-4000-8000-000000000007","result_id":"60000000-0000-4000-8000-000000000006","root_reservation_id":"80000000-0000-4000-8000-000000000008","runtime_session_id":"40000000-0000-4000-8000-000000000004","schema":1,"status":"completed","summary":"two files changed","worker_id":"90000000-0000-4000-8000-000000000009"}"#,
);

struct Fixture {
    home: PathBuf,
    store: ProtocolStore,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        // An explicit fresh authority root is stronger than mutating the process-wide
        // AGENT_RELAY_HOME: parallel integration tests cannot observe an ambient store.
        let home = fresh_home(tag);
        seed_registry(&home);
        let store = ProtocolStore::new(home.clone());
        Self { home, store }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.home).ok();
    }
}

fn registry_entry(id: &str, name: &str) -> JsonValue {
    let mut entry = HashMap::new();
    entry.insert("id".into(), JsonValue::from(id.to_string()));
    entry.insert(
        "dir".into(),
        JsonValue::from(format!("/tmp/session-relay-protocol-{name}")),
    );
    entry.insert("name".into(), JsonValue::from(name.to_string()));
    entry.insert("tool".into(), JsonValue::from("claude".to_string()));
    entry.insert("lastSeen".into(), JsonValue::from(CREATED_AT.to_string()));
    entry.insert("server".into(), JsonValue::from(()));
    entry.insert("spawned_via".into(), JsonValue::from(()));
    JsonValue::from(entry)
}

fn seed_registry(home: &Path) {
    let mut agents = HashMap::new();
    let mut names = HashMap::new();
    for (id, name) in [
        (REQUESTER_ID, "requester"),
        (RESPONDER_ID, "responder"),
        (THIRD_SESSION_ID, "third"),
    ] {
        agents.insert(id.to_string(), registry_entry(id, name));
        names.insert(name.to_string(), JsonValue::from(id.to_string()));
    }
    let root = HashMap::from([
        ("agents".to_string(), JsonValue::from(agents)),
        ("names".to_string(), JsonValue::from(names)),
    ]);
    fs::write(
        home.join("registry.json"),
        JsonValue::from(root).format().unwrap(),
    )
    .unwrap();
}

fn request_message() -> MessageV2 {
    MessageV2 {
        schema: 2,
        id: REQUEST_ID.into(),
        created_at: CREATED_AT.into(),
        from_session_id: REQUESTER_ID.into(),
        to_session_id: RESPONDER_ID.into(),
        correlation_id: CORRELATION_ID.into(),
        kind: MessageKind::Request,
        reply_to: None,
        terminal_status: None,
        body: "compile exact".into(),
        result_sha256: None,
    }
}

fn terminal_reply(status: TerminalStatus, body: &str) -> MessageV2 {
    MessageV2 {
        schema: 2,
        id: REPLY_ID.into(),
        created_at: UPDATED_AT.into(),
        from_session_id: RESPONDER_ID.into(),
        to_session_id: REQUESTER_ID.into(),
        correlation_id: CORRELATION_ID.into(),
        kind: MessageKind::TerminalReply,
        reply_to: Some(REQUEST_ID.into()),
        terminal_status: Some(status),
        body: body.into(),
        result_sha256: None,
    }
}

fn worker_result_message() -> MessageV2 {
    MessageV2 {
        schema: 2,
        id: REPLY_ID.into(),
        created_at: UPDATED_AT.into(),
        from_session_id: RESPONDER_ID.into(),
        to_session_id: REQUESTER_ID.into(),
        correlation_id: CORRELATION_ID.into(),
        kind: MessageKind::WorkerResult,
        reply_to: Some(REQUEST_ID.into()),
        terminal_status: Some(TerminalStatus::Completed),
        body: "two files changed".into(),
        result_sha256: Some(WORKER_RESULT_SHA256.into()),
    }
}

fn open_claim(origin: ClaimOrigin) -> ClaimStatusV1 {
    let request = request_message();
    ClaimStatusV1 {
        schema: 1,
        correlation_id: CORRELATION_ID.into(),
        origin,
        state: ClaimState::Open,
        requester_session_id: REQUESTER_ID.into(),
        responder_session_id: RESPONDER_ID.into(),
        request_sha256: request.sha256(),
        request,
        request_delivery: match origin {
            ClaimOrigin::Message => DeliveryState::Enqueued,
            ClaimOrigin::Fanout => DeliveryState::NotApplicable,
        },
        reply: None,
        reply_sha256: None,
        reply_delivery: None,
        created_at: CREATED_AT.into(),
        updated_at: UPDATED_AT.into(),
    }
}

fn terminal_claim(origin: ClaimOrigin) -> ClaimStatusV1 {
    let mut claim = open_claim(origin);
    let reply = match origin {
        ClaimOrigin::Message => terminal_reply(TerminalStatus::Completed, "done"),
        ClaimOrigin::Fanout => worker_result_message(),
    };
    claim.state = ClaimState::ReplyEnqueued;
    claim.reply_sha256 = Some(reply.sha256());
    claim.reply = Some(reply);
    claim.reply_delivery = Some(DeliveryState::Enqueued);
    claim
}

fn worker_result() -> WorkerResultV1 {
    WorkerResultV1 {
        schema: 1,
        result_id: RESULT_ID.into(),
        correlation_id: CORRELATION_ID.into(),
        reservation_id: RESERVATION_ID.into(),
        root_reservation_id: ROOT_RESERVATION_ID.into(),
        parent_session_id: REQUESTER_ID.into(),
        worker_id: WORKER_ID.into(),
        generation: GENERATION_ID.into(),
        runtime_session_id: RESPONDER_ID.into(),
        repo_common_dir: "/tmp/protocol-repository/.git".into(),
        repo_dev: "2049".into(),
        repo_ino: "987654".into(),
        object_format: ObjectFormat::Sha1,
        base_commit: "a".repeat(40),
        handback_commit: "b".repeat(40),
        status: TerminalStatus::Completed,
        summary: "two files changed".into(),
        changed_paths: vec!["src/protocol.rs".into(), "tests/protocol.rs".into()],
        created_at: CREATED_AT.into(),
    }
}

fn decode<T: ClosedJcs>(bytes: &[u8]) -> Result<T, String> {
    parse_jcs(bytes, false).and_then(T::from_jcs)
}

fn validate_record<T: ClosedJcs>(record: &T) -> Result<T, String> {
    T::from_jcs(record.to_jcs())
}

fn assert_closed_record_rejections<T>(record: &T, missing_key: &str)
where
    T: ClosedJcs + Debug,
{
    let mut unknown = record.to_jcs().object().unwrap();
    unknown.insert("unknown".into(), JcsValue::Null);
    assert!(
        T::from_jcs(JcsValue::Object(unknown)).is_err(),
        "{} admitted an unknown field",
        std::any::type_name::<T>()
    );

    let mut missing = record.to_jcs().object().unwrap();
    assert!(missing.remove(missing_key).is_some());
    assert!(
        T::from_jcs(JcsValue::Object(missing)).is_err(),
        "{} admitted a missing field",
        std::any::type_name::<T>()
    );

    let mut wrong_schema_type = record.to_jcs().object().unwrap();
    wrong_schema_type.insert("schema".into(), JcsValue::String("1".into()));
    assert!(T::from_jcs(JcsValue::Object(wrong_schema_type)).is_err());

    let canonical = serialize_jcs(&record.to_jcs());
    let (needle, replacement) = if canonical.contains(r#""schema":2"#) {
        (r#""schema":2"#, r#""schema":2,"schema":2"#)
    } else {
        (r#""schema":1"#, r#""schema":1,"schema":1"#)
    };
    let duplicate = canonical.replacen(needle, replacement, 1);
    assert_ne!(duplicate, canonical);
    assert!(
        parse_jcs(duplicate.as_bytes(), false)
            .and_then(T::from_jcs)
            .is_err(),
        "{} admitted a duplicate field",
        std::any::type_name::<T>()
    );

    let noncanonical = format!("{{ {}", &canonical[1..]);
    assert!(
        parse_jcs(noncanonical.as_bytes(), false)
            .and_then(T::from_jcs)
            .is_err()
    );
    let with_transport_lf = format!("{canonical}\n");
    assert!(
        parse_jcs(with_transport_lf.as_bytes(), false)
            .and_then(T::from_jcs)
            .is_err()
    );
}

fn mailbox_messages(home: &Path, recipient: &str) -> Vec<MessageV2> {
    let path = home.join("mailbox").join(format!("{recipient}.jsonl"));
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|line| decode::<MessageV2>(line.as_bytes()).expect("canonical typed mailbox row"))
        .collect()
}

fn claim_files(home: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for state in ["pending", "open", "terminal"] {
        let directory = home.join("protocol-v1").join(state);
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        paths.extend(entries.map(|entry| entry.unwrap().path()));
    }
    paths.sort();
    paths
}

fn claim_path(home: &Path, correlation_id: &str) -> PathBuf {
    claim_files(home)
        .into_iter()
        .find(|path| path.file_stem().and_then(|stem| stem.to_str()) == Some(correlation_id))
        .expect("claim path")
}

fn read_claim_file(path: &Path) -> ClaimStatusV1 {
    let bytes = fs::read(path).unwrap();
    ClaimStatusV1::from_jcs(parse_jcs(&bytes, true).unwrap()).unwrap()
}

fn write_jcs_file(path: &Path, value: JcsValue) {
    let mut bytes = serialize_jcs(&value).into_bytes();
    bytes.push(b'\n');
    fs::write(path, bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

fn write_mailbox_message(home: &Path, recipient: &str, message: &MessageV2) {
    let directory = home.join("mailbox");
    fs::create_dir_all(&directory).unwrap();
    let mut bytes = message.canonical_bytes();
    bytes.push(b'\n');
    fs::write(directory.join(format!("{recipient}.jsonl")), bytes).unwrap();
}

fn error_code(error: &ProtocolError) -> &str {
    error.code()
}

#[test]
fn message_v2_has_exact_canonical_round_trip_and_digest() {
    let message = request_message();
    assert_eq!(message.canonical_bytes(), CANONICAL_REQUEST.as_bytes());
    assert_eq!(message.sha256(), REQUEST_SHA256);
    assert_eq!(
        decode::<MessageV2>(CANONICAL_REQUEST.as_bytes()).unwrap(),
        message
    );
    assert_eq!(message.canonical_bytes().len(), 364);
}

#[test]
fn claim_status_v1_has_exact_canonical_round_trip_and_digest() {
    let claim = open_claim(ClaimOrigin::Message);
    assert_eq!(claim.request_sha256, REQUEST_SHA256);
    assert_eq!(claim.canonical_bytes(), CANONICAL_OPEN_CLAIM.as_bytes());
    assert_eq!(claim.sha256(), CLAIM_SHA256);
    assert_eq!(
        decode::<ClaimStatusV1>(CANONICAL_OPEN_CLAIM.as_bytes()).unwrap(),
        claim
    );
    assert_eq!(claim.canonical_bytes().len(), 850);
}

#[test]
fn worker_result_v1_has_exact_canonical_round_trip_and_digest() {
    let result = worker_result();
    assert_eq!(result.canonical_bytes(), CANONICAL_WORKER_RESULT.as_bytes());
    assert_eq!(result.sha256(), WORKER_RESULT_SHA256);
    assert_eq!(
        decode::<WorkerResultV1>(CANONICAL_WORKER_RESULT.as_bytes()).unwrap(),
        result
    );
    assert_eq!(result.canonical_bytes().len(), 834);
}

#[test]
fn every_protocol_record_is_closed_and_rejects_noncanonical_transport() {
    assert_closed_record_rejections(&request_message(), "body");
    assert_closed_record_rejections(&open_claim(ClaimOrigin::Message), "request");
    assert_closed_record_rejections(&worker_result(), "changed_paths");
}

#[test]
fn message_variant_matrix_is_exhaustive_and_closed() {
    let mut legal_count = 0;
    for kind in [
        MessageKind::Request,
        MessageKind::TerminalReply,
        MessageKind::WorkerResult,
    ] {
        for has_reply_to in [false, true] {
            for status in [
                None,
                Some(TerminalStatus::Completed),
                Some(TerminalStatus::Failed),
            ] {
                for has_result in [false, true] {
                    let mut message = request_message();
                    message.kind = kind;
                    if !matches!(kind, MessageKind::Request) {
                        message.from_session_id = RESPONDER_ID.into();
                        message.to_session_id = REQUESTER_ID.into();
                    }
                    message.reply_to = has_reply_to.then(|| REQUEST_ID.into());
                    message.terminal_status = status;
                    message.result_sha256 = has_result.then(|| SHA_A.into());
                    let legal = match kind {
                        MessageKind::Request => !has_reply_to && status.is_none() && !has_result,
                        MessageKind::TerminalReply => {
                            has_reply_to && status.is_some() && !has_result
                        }
                        MessageKind::WorkerResult => has_reply_to && status.is_some() && has_result,
                    };
                    legal_count += usize::from(legal);
                    assert_eq!(
                        validate_record(&message).is_ok(),
                        legal,
                        "kind={kind:?} reply_to={has_reply_to} status={status:?} result={has_result}"
                    );
                }
            }
        }
    }
    assert_eq!(legal_count, 5);

    for (field, value) in [("kind", "response"), ("terminal_status", "succeeded")] {
        let mut object = terminal_reply(TerminalStatus::Completed, "done")
            .to_jcs()
            .object()
            .unwrap();
        object.insert(field.into(), JcsValue::String(value.into()));
        assert!(MessageV2::from_jcs(JcsValue::Object(object)).is_err());
    }
}

#[test]
fn message_uuid_fields_require_lowercase_v4_with_rfc_variant() {
    let invalid = [
        "10000000-0000-1000-8000-000000000001",
        "10000000-0000-4000-c000-000000000001",
        "10000000-0000-4000-8000-00000000000G",
        "10000000000040008000000000000001",
        "A0000000-0000-4000-8000-00000000000A",
    ];
    for field in 0..4 {
        for value in invalid {
            let mut message = request_message();
            match field {
                0 => message.id = value.into(),
                1 => message.correlation_id = value.into(),
                2 => message.from_session_id = value.into(),
                _ => message.to_session_id = value.into(),
            }
            assert!(
                validate_record(&message).is_err(),
                "UUID field {field} admitted {value}"
            );
        }
    }

    for value in invalid {
        let mut reply = terminal_reply(TerminalStatus::Completed, "done");
        reply.reply_to = Some(value.into());
        assert!(
            validate_record(&reply).is_err(),
            "reply_to admitted {value}"
        );
    }
}

#[test]
fn protocol_timestamps_require_exact_real_millisecond_utc_instants() {
    for valid in ["2024-02-29T00:00:00.000Z", "2026-07-25T23:59:59.999Z"] {
        let mut message = request_message();
        message.created_at = valid.into();
        assert!(validate_record(&message).is_ok(), "rejected {valid}");
    }

    let invalid = [
        "2025-02-29T00:00:00.000Z",
        "2024-02-30T00:00:00.000Z",
        "2026-04-31T12:00:00.000Z",
        "2026-00-01T00:00:00.000Z",
        "2026-13-01T00:00:00.000Z",
        "2026-01-01T24:00:00.000Z",
        "2026-01-01T00:60:00.000Z",
        "2026-01-01T00:00:60.000Z",
        "2026-01-01T00:00:00Z",
        "2026-01-01t00:00:00.000z",
        "2026-01-01T00:00:00.000+00:00",
    ];
    for value in invalid {
        let mut message = request_message();
        message.created_at = value.into();
        assert!(validate_record(&message).is_err(), "admitted {value}");

        let mut claim = open_claim(ClaimOrigin::Message);
        claim.updated_at = value.into();
        assert!(validate_record(&claim).is_err(), "claim admitted {value}");

        let mut result = worker_result();
        result.created_at = value.into();
        assert!(validate_record(&result).is_err(), "result admitted {value}");
    }
}

#[test]
fn message_body_uses_utf8_byte_bounds_rejects_nul_and_enforces_envelope_limit() {
    for body in ["x".into(), "x".repeat(4096), "é".repeat(2048)] {
        let mut message = request_message();
        message.body = body;
        assert!(validate_record(&message).is_ok());
    }
    for body in [
        String::new(),
        "x".repeat(4097),
        "é".repeat(2049),
        "left\0right".into(),
    ] {
        let mut message = request_message();
        message.body = body;
        assert!(validate_record(&message).is_err());
    }

    let mut below_envelope_limit = request_message();
    below_envelope_limit.body = "\u{1}".repeat(2600);
    assert!(
        serialize_jcs(&below_envelope_limit.to_jcs()).len() <= 16 * 1024,
        "fixture must remain below the envelope limit"
    );
    assert!(validate_record(&below_envelope_limit).is_ok());

    let mut above_envelope_limit = request_message();
    above_envelope_limit.body = "\u{1}".repeat(2700);
    assert!(
        serialize_jcs(&above_envelope_limit.to_jcs()).len() > 16 * 1024,
        "fixture must isolate the encoded-envelope limit"
    );
    assert!(validate_record(&above_envelope_limit).is_err());
}

#[test]
fn message_digest_fields_are_lowercase_exact_sha256() {
    for digest in [
        "a".repeat(63),
        "a".repeat(65),
        "A".repeat(64),
        format!("{}g", "a".repeat(63)),
    ] {
        let mut message = worker_result_message();
        message.result_sha256 = Some(digest);
        assert!(validate_record(&message).is_err());
    }
    let mut message = worker_result_message();
    message.result_sha256 = Some(SHA_B.into());
    assert!(validate_record(&message).is_ok());
}

fn candidate_claim(
    origin: ClaimOrigin,
    state: ClaimState,
    request_delivery: DeliveryState,
    has_reply: bool,
    reply_delivery: Option<DeliveryState>,
) -> ClaimStatusV1 {
    let mut claim = open_claim(origin);
    claim.state = state;
    claim.request_delivery = request_delivery;
    claim.reply_delivery = reply_delivery;
    if has_reply {
        let reply = match origin {
            ClaimOrigin::Message => terminal_reply(TerminalStatus::Completed, "done"),
            ClaimOrigin::Fanout => worker_result_message(),
        };
        claim.reply_sha256 = Some(reply.sha256());
        claim.reply = Some(reply);
    } else {
        claim.reply = None;
        claim.reply_sha256 = None;
    }
    claim
}

fn legal_claim_matrix(
    origin: ClaimOrigin,
    state: ClaimState,
    request_delivery: DeliveryState,
    has_reply: bool,
    reply_delivery: Option<DeliveryState>,
) -> bool {
    match (origin, state) {
        (ClaimOrigin::Message, ClaimState::RequestPending) => {
            request_delivery == DeliveryState::Pending && !has_reply && reply_delivery.is_none()
        }
        (ClaimOrigin::Message, ClaimState::Open) => {
            matches!(
                request_delivery,
                DeliveryState::Enqueued | DeliveryState::Consumed
            ) && !has_reply
                && reply_delivery.is_none()
        }
        (ClaimOrigin::Fanout, ClaimState::Open) => {
            request_delivery == DeliveryState::NotApplicable
                && !has_reply
                && reply_delivery.is_none()
        }
        (origin, ClaimState::ReplyPending) => {
            legal_terminal_request_delivery(origin, request_delivery)
                && has_reply
                && reply_delivery == Some(DeliveryState::Pending)
        }
        (origin, ClaimState::ReplyEnqueued) => {
            legal_terminal_request_delivery(origin, request_delivery)
                && has_reply
                && reply_delivery == Some(DeliveryState::Enqueued)
        }
        (origin, ClaimState::ReplyConsumed) => {
            legal_terminal_request_delivery(origin, request_delivery)
                && has_reply
                && reply_delivery == Some(DeliveryState::Consumed)
        }
        _ => false,
    }
}

fn legal_terminal_request_delivery(origin: ClaimOrigin, delivery: DeliveryState) -> bool {
    match origin {
        ClaimOrigin::Message => {
            matches!(delivery, DeliveryState::Enqueued | DeliveryState::Consumed)
        }
        ClaimOrigin::Fanout => delivery == DeliveryState::NotApplicable,
    }
}

#[test]
fn claim_origin_state_and_delivery_matrix_is_exhaustive_and_closed() {
    let origins = [ClaimOrigin::Message, ClaimOrigin::Fanout];
    let states = [
        ClaimState::RequestPending,
        ClaimState::Open,
        ClaimState::ReplyPending,
        ClaimState::ReplyEnqueued,
        ClaimState::ReplyConsumed,
    ];
    let deliveries = [
        DeliveryState::Pending,
        DeliveryState::Enqueued,
        DeliveryState::Consumed,
        DeliveryState::NotApplicable,
    ];
    let reply_deliveries = [
        None,
        Some(DeliveryState::Pending),
        Some(DeliveryState::Enqueued),
        Some(DeliveryState::Consumed),
        Some(DeliveryState::NotApplicable),
    ];
    let mut legal_count = 0;
    for origin in origins {
        for state in states {
            for request_delivery in deliveries {
                for has_reply in [false, true] {
                    for reply_delivery in reply_deliveries {
                        let claim = candidate_claim(
                            origin,
                            state,
                            request_delivery,
                            has_reply,
                            reply_delivery,
                        );
                        let legal = legal_claim_matrix(
                            origin,
                            state,
                            request_delivery,
                            has_reply,
                            reply_delivery,
                        );
                        legal_count += usize::from(legal);
                        assert_eq!(
                            validate_record(&claim).is_ok(),
                            legal,
                            "origin={origin:?} state={state:?} request_delivery={request_delivery:?} has_reply={has_reply} reply_delivery={reply_delivery:?}"
                        );
                    }
                }
            }
        }
    }
    assert_eq!(legal_count, 13);
}

#[test]
fn claim_validates_request_identity_digest_and_endpoint_bindings() {
    let base = open_claim(ClaimOrigin::Message);
    assert!(validate_record(&base).is_ok());

    let mut mutations: Vec<ClaimStatusV1> = Vec::new();
    let mut claim = base.clone();
    claim.correlation_id = THIRD_SESSION_ID.into();
    mutations.push(claim);
    let mut claim = base.clone();
    claim.request.correlation_id = THIRD_SESSION_ID.into();
    mutations.push(claim);
    let mut claim = base.clone();
    claim.requester_session_id = THIRD_SESSION_ID.into();
    mutations.push(claim);
    let mut claim = base.clone();
    claim.responder_session_id = THIRD_SESSION_ID.into();
    mutations.push(claim);
    let mut claim = base.clone();
    claim.request.from_session_id = THIRD_SESSION_ID.into();
    mutations.push(claim);
    let mut claim = base.clone();
    claim.request.to_session_id = THIRD_SESSION_ID.into();
    mutations.push(claim);
    let mut claim = base.clone();
    claim.request_sha256 = SHA_A.into();
    mutations.push(claim);
    let mut claim = base.clone();
    claim.request.kind = MessageKind::TerminalReply;
    claim.request.reply_to = Some(REPLY_ID.into());
    claim.request.terminal_status = Some(TerminalStatus::Completed);
    claim.request_sha256 = claim.request.sha256();
    mutations.push(claim);

    for claim in mutations {
        assert!(
            validate_record(&claim).is_err(),
            "invalid request binding admitted"
        );
    }
}

#[test]
fn claim_validates_terminal_reply_identity_digest_and_origin_binding() {
    let message_claim = terminal_claim(ClaimOrigin::Message);
    let fanout_claim = terminal_claim(ClaimOrigin::Fanout);
    assert!(validate_record(&message_claim).is_ok());
    assert!(validate_record(&fanout_claim).is_ok());

    let mut mutations = Vec::new();
    let mut claim = message_claim.clone();
    claim.reply.as_mut().unwrap().correlation_id = THIRD_SESSION_ID.into();
    claim.reply_sha256 = Some(claim.reply.as_ref().unwrap().sha256());
    mutations.push(claim);
    let mut claim = message_claim.clone();
    claim.reply.as_mut().unwrap().from_session_id = THIRD_SESSION_ID.into();
    claim.reply_sha256 = Some(claim.reply.as_ref().unwrap().sha256());
    mutations.push(claim);
    let mut claim = message_claim.clone();
    claim.reply.as_mut().unwrap().to_session_id = THIRD_SESSION_ID.into();
    claim.reply_sha256 = Some(claim.reply.as_ref().unwrap().sha256());
    mutations.push(claim);
    let mut claim = message_claim.clone();
    claim.reply.as_mut().unwrap().reply_to = Some(REPLY_ID.into());
    claim.reply_sha256 = Some(claim.reply.as_ref().unwrap().sha256());
    mutations.push(claim);
    let mut claim = message_claim.clone();
    claim.reply_sha256 = Some(SHA_A.into());
    mutations.push(claim);
    let mut claim = message_claim.clone();
    claim.reply = Some(worker_result_message());
    claim.reply_sha256 = Some(claim.reply.as_ref().unwrap().sha256());
    mutations.push(claim);
    let mut claim = fanout_claim.clone();
    claim.reply = Some(terminal_reply(TerminalStatus::Completed, "done"));
    claim.reply_sha256 = Some(claim.reply.as_ref().unwrap().sha256());
    mutations.push(claim);

    for claim in mutations {
        assert!(
            validate_record(&claim).is_err(),
            "invalid terminal binding admitted"
        );
    }
}

#[test]
fn claim_rejects_unknown_origin_state_delivery_and_digest_syntax() {
    let base = terminal_claim(ClaimOrigin::Message);
    for (field, value) in [
        ("origin", "mailbox"),
        ("state", "Closed"),
        ("request_delivery", "queued"),
        ("reply_delivery", "delivered"),
    ] {
        let mut object = base.to_jcs().object().unwrap();
        object.insert(field.into(), JcsValue::String(value.into()));
        assert!(ClaimStatusV1::from_jcs(JcsValue::Object(object)).is_err());
    }
    for digest in [
        "f".repeat(63),
        "F".repeat(64),
        format!("{}z", "f".repeat(63)),
    ] {
        let mut claim = base.clone();
        claim.reply_sha256 = Some(digest);
        assert!(validate_record(&claim).is_err());
    }
}

#[test]
fn worker_result_uuid_decimal_format_and_oid_fields_are_closed() {
    let invalid_uuid = "10000000-0000-1000-8000-000000000001";
    for field in 0..8 {
        let mut result = worker_result();
        match field {
            0 => result.result_id = invalid_uuid.into(),
            1 => result.correlation_id = invalid_uuid.into(),
            2 => result.reservation_id = invalid_uuid.into(),
            3 => result.root_reservation_id = invalid_uuid.into(),
            4 => result.parent_session_id = invalid_uuid.into(),
            5 => result.worker_id = invalid_uuid.into(),
            6 => result.generation = invalid_uuid.into(),
            _ => result.runtime_session_id = invalid_uuid.into(),
        }
        assert!(
            validate_record(&result).is_err(),
            "UUID field {field} was open"
        );
    }

    for value in ["", "01", "+1", "-1", " 1", "1.0", "18446744073709551616"] {
        let mut result = worker_result();
        result.repo_dev = value.into();
        assert!(
            validate_record(&result).is_err(),
            "repo_dev admitted {value}"
        );
        let mut result = worker_result();
        result.repo_ino = value.into();
        assert!(
            validate_record(&result).is_err(),
            "repo_ino admitted {value}"
        );
    }
    for value in ["0", "1", "18446744073709551615"] {
        let mut result = worker_result();
        result.repo_dev = value.into();
        result.repo_ino = value.into();
        assert!(validate_record(&result).is_ok(), "rejected decimal {value}");
    }

    let mut sha256_result = worker_result();
    sha256_result.object_format = ObjectFormat::Sha256;
    sha256_result.base_commit = "c".repeat(64);
    sha256_result.handback_commit = "d".repeat(64);
    assert!(validate_record(&sha256_result).is_ok());

    for (format, oid) in [
        (ObjectFormat::Sha1, "a".repeat(39)),
        (ObjectFormat::Sha1, "a".repeat(64)),
        (ObjectFormat::Sha1, "A".repeat(40)),
        (ObjectFormat::Sha256, "a".repeat(40)),
        (ObjectFormat::Sha256, "a".repeat(63)),
        (ObjectFormat::Sha256, format!("{}g", "a".repeat(63))),
    ] {
        let mut result = worker_result();
        result.object_format = format;
        result.base_commit = oid;
        result.handback_commit = match format {
            ObjectFormat::Sha1 => "b".repeat(40),
            ObjectFormat::Sha256 => "b".repeat(64),
        };
        assert!(validate_record(&result).is_err());
    }

    for (field, value) in [("object_format", "sha512"), ("status", "succeeded")] {
        let mut object = worker_result().to_jcs().object().unwrap();
        object.insert(field.into(), JcsValue::String(value.into()));
        assert!(WorkerResultV1::from_jcs(JcsValue::Object(object)).is_err());
    }
}

#[test]
fn worker_result_summary_uses_utf8_byte_bounds_and_rejects_nul() {
    for summary in [String::new(), "x".repeat(4096), "é".repeat(2048)] {
        let mut result = worker_result();
        result.summary = summary;
        assert!(validate_record(&result).is_ok());
    }
    for summary in ["x".repeat(4097), "é".repeat(2049), "left\0right".into()] {
        let mut result = worker_result();
        result.summary = summary;
        assert!(validate_record(&result).is_err());
    }
    let mut failed = worker_result();
    failed.status = TerminalStatus::Failed;
    assert!(validate_record(&failed).is_ok());
}

#[test]
fn worker_result_paths_are_sorted_unique_normalized_git_relative_paths() {
    for valid in [
        Vec::<String>::new(),
        vec!["a".into()],
        vec![".github/workflows/ci.yml".into(), "docs/é.txt".into()],
    ] {
        let mut result = worker_result();
        result.changed_paths = valid;
        assert!(validate_record(&result).is_ok());
    }

    let invalid = [
        vec!["".into()],
        vec!["/absolute".into()],
        vec!["./relative".into()],
        vec!["../escape".into()],
        vec!["a/./b".into()],
        vec!["a/../b".into()],
        vec!["a//b".into()],
        vec!["trailing/".into()],
        vec!["left\0right".into()],
        vec!["a".into(), "a".into()],
        vec!["z".into(), "a".into()],
    ];
    for paths in invalid {
        let mut result = worker_result();
        result.changed_paths = paths;
        assert!(validate_record(&result).is_err());
    }

    let mut at_limit = worker_result();
    at_limit.changed_paths = (0..4096).map(|index| format!("p/{index:04}")).collect();
    assert!(validate_record(&at_limit).is_ok());
    at_limit.changed_paths.push("z/last".into());
    assert!(validate_record(&at_limit).is_err());
}

#[test]
fn worker_result_common_dir_is_an_absolute_lexically_canonical_path() {
    for invalid in [
        "",
        "relative/.git",
        "/tmp/repo/../repo/.git",
        "/tmp/repo/./.git",
        "/tmp/repo//.git",
        "/tmp/repo/.git/",
        "/tmp/repo/\0.git",
    ] {
        let mut result = worker_result();
        result.repo_common_dir = invalid.into();
        assert!(validate_record(&result).is_err(), "admitted {invalid:?}");
    }
}

#[test]
fn worker_result_enforces_the_one_mib_canonical_encoded_limit() {
    let mut below_limit = worker_result();
    below_limit.changed_paths = (0..4096)
        .map(|index| format!("{index:04}/{}", "x".repeat(240)))
        .collect();
    assert!(serialize_jcs(&below_limit.to_jcs()).len() <= 1024 * 1024);
    assert!(validate_record(&below_limit).is_ok());

    let mut above_limit = worker_result();
    above_limit.changed_paths = (0..4096)
        .map(|index| format!("{index:04}/{}", "x".repeat(250)))
        .collect();
    assert!(serialize_jcs(&above_limit.to_jcs()).len() > 1024 * 1024);
    assert!(validate_record(&above_limit).is_err());
}

#[test]
fn request_requires_exact_registered_endpoints_and_persists_one_open_claim() {
    let fixture = Fixture::new("protocol-request-registration");
    let request = fixture
        .store
        .request(REQUESTER_ID, RESPONDER_ID, "perform the task")
        .unwrap();
    assert_eq!(request.schema, 2);
    assert_eq!(request.kind, MessageKind::Request);
    assert_eq!(request.from_session_id, REQUESTER_ID);
    assert_eq!(request.to_session_id, RESPONDER_ID);
    assert!(LowerUuidV4::parse(&request.id).is_ok());
    assert!(LowerUuidV4::parse(&request.correlation_id).is_ok());
    assert_eq!(
        fixture.store.peek_typed(RESPONDER_ID).unwrap(),
        vec![request.clone()]
    );

    let claim = fixture
        .store
        .read_claim(&request.correlation_id)
        .unwrap()
        .unwrap();
    assert_eq!(claim.state, ClaimState::Open);
    assert_eq!(claim.origin, ClaimOrigin::Message);
    assert_eq!(claim.request_delivery, DeliveryState::Enqueued);
    assert_eq!(claim.request, request);
    assert_eq!(claim.request_sha256, claim.request.sha256());

    let path = claim_path(&fixture.home, &claim.correlation_id);
    assert_eq!(path.parent().unwrap().file_name().unwrap(), "open");
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
        0o600
    );
    let mut expected_file = claim.canonical_bytes();
    expected_file.push(b'\n');
    assert_eq!(fs::read(path).unwrap(), expected_file);

    let before = claim_files(&fixture.home);
    assert!(
        fixture
            .store
            .request(UNKNOWN_SESSION_ID, RESPONDER_ID, "not registered")
            .is_err()
    );
    assert!(
        fixture
            .store
            .request(REQUESTER_ID, UNKNOWN_SESSION_ID, "not registered")
            .is_err()
    );
    assert_eq!(claim_files(&fixture.home), before);
    assert!(mailbox_messages(&fixture.home, UNKNOWN_SESSION_ID).is_empty());
}

#[test]
fn exact_responder_is_authorized_and_unknown_or_other_responders_fail_closed() {
    let fixture = Fixture::new("protocol-exact-responder");
    let request = fixture
        .store
        .request(REQUESTER_ID, RESPONDER_ID, "perform the task")
        .unwrap();

    let unknown = fixture
        .store
        .reply(
            UNKNOWN_SESSION_ID,
            RESPONDER_ID,
            TerminalStatus::Completed,
            "done",
        )
        .unwrap_err();
    assert_eq!(error_code(&unknown), "unknown_correlation");

    let unauthorized = fixture
        .store
        .reply(
            &request.correlation_id,
            THIRD_SESSION_ID,
            TerminalStatus::Completed,
            "done",
        )
        .unwrap_err();
    assert_eq!(error_code(&unauthorized), "unauthorized_responder");
    assert!(mailbox_messages(&fixture.home, REQUESTER_ID).is_empty());
    assert_eq!(
        fixture
            .store
            .read_claim(&request.correlation_id)
            .unwrap()
            .unwrap()
            .state,
        ClaimState::Open
    );

    let reply = fixture
        .store
        .reply(
            &request.correlation_id,
            RESPONDER_ID,
            TerminalStatus::Completed,
            "done",
        )
        .unwrap();
    assert_eq!(reply.disposition, ReplyDisposition::Created);
}

#[test]
fn exact_reply_retry_is_idempotent_but_changed_payload_conflicts() {
    let fixture = Fixture::new("protocol-reply-idempotence");
    let request = fixture
        .store
        .request(REQUESTER_ID, RESPONDER_ID, "perform the task")
        .unwrap();
    let first = fixture
        .store
        .reply(
            &request.correlation_id,
            RESPONDER_ID,
            TerminalStatus::Completed,
            "done",
        )
        .unwrap();
    let retry = fixture
        .store
        .reply(
            &request.correlation_id,
            RESPONDER_ID,
            TerminalStatus::Completed,
            "done",
        )
        .unwrap();
    assert_eq!(first.disposition, ReplyDisposition::Created);
    assert_eq!(retry.disposition, ReplyDisposition::Idempotent);
    assert_eq!(retry.message, first.message);
    assert_eq!(
        retry.message.canonical_bytes(),
        first.message.canonical_bytes()
    );
    assert_eq!(
        mailbox_messages(&fixture.home, REQUESTER_ID),
        vec![first.message.clone()]
    );

    for (status, body) in [
        (TerminalStatus::Completed, "changed"),
        (TerminalStatus::Failed, "done"),
    ] {
        let conflict = fixture
            .store
            .reply(&request.correlation_id, RESPONDER_ID, status, body)
            .unwrap_err();
        assert_eq!(error_code(&conflict), "correlation_conflict");
    }
    assert_eq!(
        mailbox_messages(&fixture.home, REQUESTER_ID),
        vec![first.message]
    );
}

#[test]
fn concurrent_competing_terminal_claims_have_one_logical_winner() {
    let fixture = Fixture::new("protocol-reply-race");
    let request = fixture
        .store
        .request(REQUESTER_ID, RESPONDER_ID, "perform the task")
        .unwrap();
    let competitors = 16;
    let barrier = Arc::new(Barrier::new(competitors + 1));
    let mut joins = Vec::new();
    for index in 0..competitors {
        let barrier = Arc::clone(&barrier);
        let home = fixture.home.clone();
        let correlation_id = request.correlation_id.clone();
        joins.push(thread::spawn(move || {
            let store = ProtocolStore::new(home);
            barrier.wait();
            store.reply(
                &correlation_id,
                RESPONDER_ID,
                TerminalStatus::Completed,
                &format!("candidate-{index:02}"),
            )
        }));
    }
    barrier.wait();
    let results = joins
        .into_iter()
        .map(|join| join.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Ok(outcome) if outcome.disposition == ReplyDisposition::Created))
            .count(),
        1
    );
    assert_eq!(
        results
            .iter()
            .filter(
                |result| matches!(result, Err(error) if error_code(error) == "correlation_conflict")
            )
            .count(),
        competitors - 1
    );

    let messages = mailbox_messages(&fixture.home, REQUESTER_ID);
    assert_eq!(messages.len(), 1);
    let claim = fixture
        .store
        .read_claim(&request.correlation_id)
        .unwrap()
        .unwrap();
    assert_eq!(claim.state, ClaimState::ReplyEnqueued);
    assert_eq!(claim.reply.as_ref(), messages.first());
    let reply_digest = messages[0].sha256();
    assert_eq!(claim.reply_sha256.as_deref(), Some(reply_digest.as_str()));
}

#[test]
fn request_crash_failpoints_recover_without_loss_or_duplicate_delivery() {
    let points = [
        ProtocolFailpoint::RequestBeforePendingWrite,
        ProtocolFailpoint::RequestAfterPendingWrite,
        ProtocolFailpoint::RequestBeforeMailboxAppend,
        ProtocolFailpoint::RequestAfterMailboxAppend,
        ProtocolFailpoint::RequestBeforeOpenMove,
        ProtocolFailpoint::RequestOpenMoveBeforeSourceUnlink,
        ProtocolFailpoint::RequestAfterOpenMove,
    ];
    for (index, point) in points.into_iter().enumerate() {
        let fixture = Fixture::new(&format!("protocol-request-failpoint-{index}"));
        let faulted = ProtocolStore::new(fixture.home.clone()).with_failpoint(point);
        assert!(
            faulted
                .request(REQUESTER_ID, RESPONDER_ID, "recover exact request")
                .is_err(),
            "{point:?} did not interrupt"
        );
        fixture.store.recover_pending().unwrap();
        fixture.store.recover_pending().unwrap();

        if point == ProtocolFailpoint::RequestBeforePendingWrite {
            assert!(claim_files(&fixture.home).is_empty());
            assert!(mailbox_messages(&fixture.home, RESPONDER_ID).is_empty());
            continue;
        }
        let paths = claim_files(&fixture.home);
        assert_eq!(paths.len(), 1, "{point:?}");
        let claim = read_claim_file(&paths[0]);
        assert_eq!(claim.state, ClaimState::Open, "{point:?}");
        assert_eq!(claim.request_delivery, DeliveryState::Enqueued);
        assert_eq!(
            mailbox_messages(&fixture.home, RESPONDER_ID),
            vec![claim.request]
        );
    }
}

#[test]
fn reply_crash_failpoints_recover_without_loss_or_duplicate_delivery() {
    let points = [
        ProtocolFailpoint::ReplyBeforePendingWrite,
        ProtocolFailpoint::ReplyPendingMoveBeforeSourceUnlink,
        ProtocolFailpoint::ReplyAfterPendingWrite,
        ProtocolFailpoint::ReplyBeforeMailboxAppend,
        ProtocolFailpoint::ReplyAfterMailboxAppend,
        ProtocolFailpoint::ReplyBeforeEnqueuedMove,
        ProtocolFailpoint::ReplyEnqueuedMoveBeforeSourceUnlink,
        ProtocolFailpoint::ReplyAfterEnqueuedMove,
    ];
    for (index, point) in points.into_iter().enumerate() {
        let fixture = Fixture::new(&format!("protocol-reply-failpoint-{index}"));
        let request = fixture
            .store
            .request(REQUESTER_ID, RESPONDER_ID, "perform the task")
            .unwrap();
        assert_eq!(
            fixture.store.drain_typed(RESPONDER_ID).unwrap(),
            vec![request.clone()]
        );
        let faulted = ProtocolStore::new(fixture.home.clone()).with_failpoint(point);
        assert!(
            faulted
                .reply(
                    &request.correlation_id,
                    RESPONDER_ID,
                    TerminalStatus::Completed,
                    "recover exact reply",
                )
                .is_err(),
            "{point:?} did not interrupt"
        );
        fixture.store.recover_pending().unwrap();
        fixture.store.recover_pending().unwrap();

        let claim = fixture
            .store
            .read_claim(&request.correlation_id)
            .unwrap()
            .unwrap();
        if point == ProtocolFailpoint::ReplyBeforePendingWrite {
            assert_eq!(claim.state, ClaimState::Open);
            assert!(claim.reply.is_none());
            assert!(mailbox_messages(&fixture.home, REQUESTER_ID).is_empty());
            continue;
        }
        assert_eq!(claim.state, ClaimState::ReplyEnqueued, "{point:?}");
        assert_eq!(claim.reply_delivery, Some(DeliveryState::Enqueued));
        let messages = mailbox_messages(&fixture.home, REQUESTER_ID);
        assert_eq!(messages.len(), 1, "{point:?}");
        assert_eq!(claim.reply.as_ref(), messages.first());
        let reply_digest = messages[0].sha256();
        assert_eq!(claim.reply_sha256.as_deref(), Some(reply_digest.as_str()));
    }
}

#[test]
fn pending_recovery_replays_embedded_exact_bytes_and_deduplicates_existing_append() {
    for (index, point) in [
        ProtocolFailpoint::RequestAfterPendingWrite,
        ProtocolFailpoint::RequestAfterMailboxAppend,
    ]
    .into_iter()
    .enumerate()
    {
        let fixture = Fixture::new(&format!("protocol-pending-exact-{index}"));
        let faulted = ProtocolStore::new(fixture.home.clone()).with_failpoint(point);
        assert!(
            faulted
                .request(REQUESTER_ID, RESPONDER_ID, "embedded envelope")
                .is_err()
        );
        let pending_path = claim_files(&fixture.home).pop().unwrap();
        assert_eq!(
            pending_path.parent().unwrap().file_name().unwrap(),
            "pending"
        );
        let pending = read_claim_file(&pending_path);
        let exact = pending.request.canonical_bytes();

        fixture.store.recover_pending().unwrap();
        fixture.store.recover_pending().unwrap();
        let messages = mailbox_messages(&fixture.home, RESPONDER_ID);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].canonical_bytes(), exact);
        assert_eq!(messages[0].sha256(), pending.request_sha256);
        assert_eq!(
            fixture
                .store
                .read_claim(&pending.correlation_id)
                .unwrap()
                .unwrap()
                .state,
            ClaimState::Open
        );
    }
}

#[test]
fn typed_drain_marks_delivery_consumed_before_removal_and_recovery_cannot_redeliver() {
    let fixture = Fixture::new("protocol-consumed-delivery");
    let request = fixture
        .store
        .request(REQUESTER_ID, RESPONDER_ID, "perform the task")
        .unwrap();
    assert_eq!(
        fixture.store.peek_typed(RESPONDER_ID).unwrap(),
        vec![request.clone()]
    );
    assert_eq!(
        fixture
            .store
            .read_claim(&request.correlation_id)
            .unwrap()
            .unwrap()
            .request_delivery,
        DeliveryState::Enqueued
    );
    assert_eq!(
        fixture.store.drain_typed(RESPONDER_ID).unwrap(),
        vec![request.clone()]
    );
    assert!(fixture.store.peek_typed(RESPONDER_ID).unwrap().is_empty());
    assert_eq!(
        fixture
            .store
            .read_claim(&request.correlation_id)
            .unwrap()
            .unwrap()
            .request_delivery,
        DeliveryState::Consumed
    );
    fixture.store.recover_pending().unwrap();
    assert!(mailbox_messages(&fixture.home, RESPONDER_ID).is_empty());

    let reply = fixture
        .store
        .reply(
            &request.correlation_id,
            RESPONDER_ID,
            TerminalStatus::Completed,
            "done",
        )
        .unwrap()
        .message;
    assert_eq!(
        fixture.store.drain_typed(REQUESTER_ID).unwrap(),
        vec![reply]
    );
    let consumed = fixture
        .store
        .read_claim(&request.correlation_id)
        .unwrap()
        .unwrap();
    assert_eq!(consumed.state, ClaimState::ReplyConsumed);
    assert_eq!(consumed.reply_delivery, Some(DeliveryState::Consumed));
    fixture.store.recover_pending().unwrap();
    assert!(mailbox_messages(&fixture.home, REQUESTER_ID).is_empty());
}

#[test]
fn malformed_tampered_or_wrong_mode_claims_fail_closed_without_mailbox_mutation() {
    {
        let fixture = Fixture::new("protocol-tamper-unknown");
        let request = fixture
            .store
            .request(REQUESTER_ID, RESPONDER_ID, "perform the task")
            .unwrap();
        let path = claim_path(&fixture.home, &request.correlation_id);
        let mut object = parse_jcs(&fs::read(&path).unwrap(), true)
            .unwrap()
            .object()
            .unwrap();
        object.insert("unknown".into(), JcsValue::Null);
        write_jcs_file(&path, JcsValue::Object(object));
        let before = fs::read(
            fixture
                .home
                .join("mailbox")
                .join(format!("{RESPONDER_ID}.jsonl")),
        )
        .unwrap();
        let error = fixture
            .store
            .read_claim(&request.correlation_id)
            .unwrap_err();
        assert_eq!(error_code(&error), "protocol_store_error");
        assert_eq!(
            fs::read(
                fixture
                    .home
                    .join("mailbox")
                    .join(format!("{RESPONDER_ID}.jsonl"))
            )
            .unwrap(),
            before
        );
    }
    {
        let fixture = Fixture::new("protocol-tamper-digest");
        let request = fixture
            .store
            .request(REQUESTER_ID, RESPONDER_ID, "perform the task")
            .unwrap();
        let path = claim_path(&fixture.home, &request.correlation_id);
        let mut object = parse_jcs(&fs::read(&path).unwrap(), true)
            .unwrap()
            .object()
            .unwrap();
        object.insert("request_sha256".into(), JcsValue::String(SHA_A.into()));
        write_jcs_file(&path, JcsValue::Object(object));
        let error = fixture
            .store
            .read_claim(&request.correlation_id)
            .unwrap_err();
        assert_eq!(error_code(&error), "protocol_store_error");
    }
    {
        let fixture = Fixture::new("protocol-tamper-mode");
        let request = fixture
            .store
            .request(REQUESTER_ID, RESPONDER_ID, "perform the task")
            .unwrap();
        let path = claim_path(&fixture.home, &request.correlation_id);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let error = fixture
            .store
            .read_claim(&request.correlation_id)
            .unwrap_err();
        assert_eq!(error_code(&error), "protocol_store_error");
    }
    {
        let fixture = Fixture::new("protocol-tamper-directory-state");
        let request = fixture
            .store
            .request(REQUESTER_ID, RESPONDER_ID, "perform the task")
            .unwrap();
        let path = claim_path(&fixture.home, &request.correlation_id);
        let pending = fixture
            .home
            .join("protocol-v1/pending")
            .join(path.file_name().unwrap());
        fs::rename(path, pending).unwrap();
        let error = fixture.store.recover_pending().unwrap_err();
        assert_eq!(error_code(&error), "protocol_store_error");
        assert_eq!(mailbox_messages(&fixture.home, RESPONDER_ID).len(), 1);
    }
}

#[test]
fn recovery_rejects_same_message_id_with_different_bytes() {
    let fixture = Fixture::new("protocol-recovery-byte-tamper");
    let faulted = ProtocolStore::new(fixture.home.clone())
        .with_failpoint(ProtocolFailpoint::RequestAfterPendingWrite);
    assert!(
        faulted
            .request(REQUESTER_ID, RESPONDER_ID, "authoritative body")
            .is_err()
    );
    let pending = read_claim_file(&claim_files(&fixture.home)[0]);
    let mut planted = pending.request.clone();
    planted.body = "tampered body".into();
    assert!(validate_record(&planted).is_ok());
    assert_eq!(planted.id, pending.request.id);
    assert_ne!(planted.canonical_bytes(), pending.request.canonical_bytes());
    write_mailbox_message(&fixture.home, RESPONDER_ID, &planted);

    let error = fixture.store.recover_pending().unwrap_err();
    assert_eq!(error_code(&error), "protocol_store_error");
    assert_eq!(mailbox_messages(&fixture.home, RESPONDER_ID), vec![planted]);
    let retained = read_claim_file(&claim_files(&fixture.home)[0]);
    assert_eq!(retained.state, ClaimState::RequestPending);
    assert_eq!(retained.request, pending.request);
}

#[test]
fn explicit_protocol_roots_are_fresh_store_isolated() {
    let first = Fixture::new("protocol-isolation-first");
    let second = Fixture::new("protocol-isolation-second");
    assert_ne!(first.home, second.home);

    let first_request = first
        .store
        .request(REQUESTER_ID, RESPONDER_ID, "first store")
        .unwrap();
    assert!(
        second
            .store
            .read_claim(&first_request.correlation_id)
            .unwrap()
            .is_none()
    );
    assert!(second.store.peek_typed(RESPONDER_ID).unwrap().is_empty());
    assert!(claim_files(&second.home).is_empty());

    let second_request = second
        .store
        .request(REQUESTER_ID, RESPONDER_ID, "second store")
        .unwrap();
    assert_eq!(
        mailbox_messages(&first.home, RESPONDER_ID),
        vec![first_request]
    );
    assert_eq!(
        mailbox_messages(&second.home, RESPONDER_ID),
        vec![second_request]
    );
    assert_eq!(claim_files(&first.home).len(), 1);
    assert_eq!(claim_files(&second.home).len(), 1);
}

fn lifecycle_drain(home: &Path, session: &str) -> Result<relay::store::DrainReceipt, String> {
    let store = LifecycleStore::new(home.to_path_buf());
    let Admission::Unmanaged(mut guard) =
        store.admit_operation(session, OperationKind::CliInboxDrain)?
    else {
        panic!("expected unmanaged admission");
    };
    lifecycle::drain_with_guard(&mut guard)
}

fn claim_file_in(home: &Path, directory: &str) -> PathBuf {
    claim_files(home)
        .into_iter()
        .find(|path| path.parent().unwrap().file_name().unwrap() == directory)
        .unwrap_or_else(|| panic!("no claim file in {directory}"))
}

#[test]
fn interrupted_request_move_after_destination_write_recovers_deterministically() {
    let fixture = Fixture::new("protocol-move-unlink-request");
    let faulted = ProtocolStore::new(fixture.home.clone())
        .with_failpoint(ProtocolFailpoint::RequestOpenMoveBeforeSourceUnlink);
    assert!(
        faulted
            .request(REQUESTER_ID, RESPONDER_ID, "converge exact request")
            .is_err()
    );

    // The crash window leaves the stale source beside the authoritative
    // destination.
    let stale = read_claim_file(&claim_file_in(&fixture.home, "pending"));
    let authoritative = read_claim_file(&claim_file_in(&fixture.home, "open"));
    assert_eq!(stale.state, ClaimState::RequestPending);
    assert_eq!(authoritative.state, ClaimState::Open);
    let correlation_id = stale.correlation_id.clone();

    // Read-side resolution returns the later state without reconciling files.
    let resolved = fixture.store.read_claim(&correlation_id).unwrap().unwrap();
    assert_eq!(resolved.state, ClaimState::Open);
    assert_eq!(resolved.request_delivery, DeliveryState::Enqueued);
    assert_eq!(claim_files(&fixture.home).len(), 2);

    // Recovery converges idempotently to exactly one claim and one row.
    fixture.store.recover_pending().unwrap();
    fixture.store.recover_pending().unwrap();
    let paths = claim_files(&fixture.home);
    assert_eq!(paths.len(), 1);
    let converged = read_claim_file(&paths[0]);
    assert_eq!(converged.state, ClaimState::Open);
    assert_eq!(converged.request_delivery, DeliveryState::Enqueued);
    assert_eq!(
        mailbox_messages(&fixture.home, RESPONDER_ID),
        vec![converged.request.clone()]
    );
    assert_eq!(
        fixture.store.drain_typed(RESPONDER_ID).unwrap(),
        vec![converged.request]
    );
    assert!(fixture.store.drain_typed(RESPONDER_ID).unwrap().is_empty());
}

#[test]
fn interrupted_reply_moves_after_destination_write_recover_deterministically() {
    for (index, point) in [
        ProtocolFailpoint::ReplyPendingMoveBeforeSourceUnlink,
        ProtocolFailpoint::ReplyEnqueuedMoveBeforeSourceUnlink,
    ]
    .into_iter()
    .enumerate()
    {
        let fixture = Fixture::new(&format!("protocol-move-unlink-reply-{index}"));
        let request = fixture
            .store
            .request(REQUESTER_ID, RESPONDER_ID, "perform the task")
            .unwrap();
        assert_eq!(
            fixture.store.drain_typed(RESPONDER_ID).unwrap(),
            vec![request.clone()]
        );
        let faulted = ProtocolStore::new(fixture.home.clone()).with_failpoint(point);
        assert!(
            faulted
                .reply(
                    &request.correlation_id,
                    RESPONDER_ID,
                    TerminalStatus::Completed,
                    "converge exact reply",
                )
                .is_err(),
            "{point:?} did not interrupt"
        );
        assert_eq!(claim_files(&fixture.home).len(), 2, "{point:?}");

        // Read-side resolution returns the later Pending/Terminal state
        // without reconciling files.
        let resolved = fixture
            .store
            .read_claim(&request.correlation_id)
            .unwrap()
            .unwrap();
        if point == ProtocolFailpoint::ReplyPendingMoveBeforeSourceUnlink {
            assert_eq!(resolved.state, ClaimState::ReplyPending);
            assert!(mailbox_messages(&fixture.home, REQUESTER_ID).is_empty());
        } else {
            assert_eq!(resolved.state, ClaimState::ReplyEnqueued);
            assert_eq!(mailbox_messages(&fixture.home, REQUESTER_ID).len(), 1);
        }
        assert_eq!(claim_files(&fixture.home).len(), 2, "{point:?}");

        fixture.store.recover_pending().unwrap();
        fixture.store.recover_pending().unwrap();
        let paths = claim_files(&fixture.home);
        assert_eq!(paths.len(), 1, "{point:?}");
        let converged = read_claim_file(&paths[0]);
        assert_eq!(converged.state, ClaimState::ReplyEnqueued, "{point:?}");
        assert_eq!(converged.reply_delivery, Some(DeliveryState::Enqueued));
        let replies = mailbox_messages(&fixture.home, REQUESTER_ID);
        assert_eq!(replies.len(), 1, "{point:?}");
        assert_eq!(converged.reply.as_ref(), replies.first());

        // The converged claim keeps the exact idempotent-retry contract and
        // delivers exactly once.
        let retry = fixture
            .store
            .reply(
                &request.correlation_id,
                RESPONDER_ID,
                TerminalStatus::Completed,
                "converge exact reply",
            )
            .unwrap();
        assert_eq!(retry.disposition, ReplyDisposition::Idempotent);
        assert_eq!(fixture.store.drain_typed(REQUESTER_ID).unwrap(), replies);
        assert!(fixture.store.drain_typed(REQUESTER_ID).unwrap().is_empty());
    }
}

#[test]
fn conflicting_duplicate_claims_fail_closed_without_convergence_or_append() {
    let fixture = Fixture::new("protocol-duplicate-conflict");
    fixture.store.recover_pending().unwrap();
    let open = open_claim(ClaimOrigin::Message);
    let mut conflicting_request = request_message();
    conflicting_request.body = "conflicting body".into();
    let pending = ClaimStatusV1 {
        state: ClaimState::RequestPending,
        request_delivery: DeliveryState::Pending,
        request_sha256: conflicting_request.sha256(),
        request: conflicting_request,
        ..open_claim(ClaimOrigin::Message)
    };
    assert!(validate_record(&pending).is_ok());
    write_jcs_file(
        &fixture
            .home
            .join("protocol-v1/open")
            .join(format!("{CORRELATION_ID}.json")),
        open.to_jcs(),
    );
    write_jcs_file(
        &fixture
            .home
            .join("protocol-v1/pending")
            .join(format!("{CORRELATION_ID}.json")),
        pending.to_jcs(),
    );

    let read_error = fixture.store.read_claim(CORRELATION_ID).unwrap_err();
    assert_eq!(error_code(&read_error), "protocol_store_error");
    let recover_error = fixture.store.recover_pending().unwrap_err();
    assert_eq!(error_code(&recover_error), "protocol_store_error");
    // Nothing converged and nothing was appended.
    assert_eq!(claim_files(&fixture.home).len(), 2);
    assert!(mailbox_messages(&fixture.home, RESPONDER_ID).is_empty());
}

#[test]
fn typed_rows_without_authoritative_claims_are_never_surfaced() {
    let fixture = Fixture::new("protocol-unclaimed-typed-row");
    write_mailbox_message(&fixture.home, RESPONDER_ID, &request_message());
    let mailbox = fixture
        .home
        .join("mailbox")
        .join(format!("{RESPONDER_ID}.jsonl"));
    let before = fs::read(&mailbox).unwrap();

    let peek_error = fixture.store.peek_typed(RESPONDER_ID).unwrap_err();
    assert_eq!(error_code(&peek_error), "protocol_store_error");
    let drain_error = fixture.store.drain_typed(RESPONDER_ID).unwrap_err();
    assert_eq!(error_code(&drain_error), "protocol_store_error");
    let renderable_error = match lifecycle_drain(&fixture.home, RESPONDER_ID) {
        Err(error) => error,
        Ok(_) => panic!("renderable drain surfaced an unclaimed typed row"),
    };
    assert!(
        renderable_error.contains("typed mailbox row has no claim"),
        "{renderable_error}"
    );
    // The refused drains leave the mailbox untouched.
    assert_eq!(fs::read(&mailbox).unwrap(), before);
}

#[test]
fn requeued_request_row_after_pre_inject_failure_redelivers_exactly_once() {
    let fixture = Fixture::new("protocol-requeue-request");
    let request = fixture
        .store
        .request(REQUESTER_ID, RESPONDER_ID, "perform the task")
        .unwrap();
    let mailbox = fixture
        .home
        .join("mailbox")
        .join(format!("{RESPONDER_ID}.jsonl"));
    let legacy_line =
        "{\"id\":\"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa\",\"body\":\"legacy first\"}\n";
    fs::OpenOptions::new()
        .append(true)
        .open(&mailbox)
        .unwrap()
        .write_all(legacy_line.as_bytes())
        .unwrap();
    let before = fs::read_to_string(&mailbox).unwrap();

    let receipt = lifecycle_drain(&fixture.home, RESPONDER_ID).unwrap();
    assert_eq!(receipt.messages().len(), 2);
    assert!(!mailbox.exists());
    assert_eq!(
        fixture
            .store
            .read_claim(&request.correlation_id)
            .unwrap()
            .unwrap()
            .request_delivery,
        DeliveryState::Consumed
    );

    // Documented pre-inject failure: the requeue restores the raw bytes AND
    // the claim delivery state, so the row stays deliverable.
    receipt.rollback().unwrap();
    assert_eq!(fs::read_to_string(&mailbox).unwrap(), before);
    assert_eq!(
        fixture
            .store
            .read_claim(&request.correlation_id)
            .unwrap()
            .unwrap()
            .request_delivery,
        DeliveryState::Enqueued
    );
    assert_eq!(
        fixture.store.peek_typed(RESPONDER_ID).unwrap(),
        vec![request.clone()]
    );

    // The retry delivers the typed row exactly once, in order, with the
    // legacy row preserved byte-for-byte.
    let retried = lifecycle_drain(&fixture.home, RESPONDER_ID).unwrap();
    let rows = retried.messages().to_vec();
    retried.commit();
    assert_eq!(rows.len(), 2);
    let typed_row = String::from_utf8(request.canonical_bytes())
        .unwrap()
        .parse::<JsonValue>()
        .unwrap();
    assert_eq!(rows[0], typed_row);
    assert_eq!(
        rows[1],
        legacy_line.trim_end().parse::<JsonValue>().unwrap()
    );
    assert_eq!(
        fixture
            .store
            .read_claim(&request.correlation_id)
            .unwrap()
            .unwrap()
            .request_delivery,
        DeliveryState::Consumed
    );
    assert!(!mailbox.exists());
    assert!(
        lifecycle_drain(&fixture.home, RESPONDER_ID)
            .unwrap()
            .messages()
            .is_empty()
    );
}

#[test]
fn requeued_terminal_reply_restores_consumed_claim_for_exact_redelivery() {
    let fixture = Fixture::new("protocol-requeue-reply");
    let request = fixture
        .store
        .request(REQUESTER_ID, RESPONDER_ID, "perform the task")
        .unwrap();
    assert_eq!(
        fixture.store.drain_typed(RESPONDER_ID).unwrap(),
        vec![request.clone()]
    );
    let reply = fixture
        .store
        .reply(
            &request.correlation_id,
            RESPONDER_ID,
            TerminalStatus::Completed,
            "done",
        )
        .unwrap()
        .message;

    let receipt = lifecycle_drain(&fixture.home, REQUESTER_ID).unwrap();
    assert_eq!(receipt.messages().len(), 1);
    assert_eq!(
        fixture
            .store
            .read_claim(&request.correlation_id)
            .unwrap()
            .unwrap()
            .state,
        ClaimState::ReplyConsumed
    );

    receipt.rollback().unwrap();
    let restored = fixture
        .store
        .read_claim(&request.correlation_id)
        .unwrap()
        .unwrap();
    assert_eq!(restored.state, ClaimState::ReplyEnqueued);
    assert_eq!(restored.reply_delivery, Some(DeliveryState::Enqueued));
    assert_eq!(
        fixture.store.peek_typed(REQUESTER_ID).unwrap(),
        vec![reply.clone()]
    );

    let retried = lifecycle_drain(&fixture.home, REQUESTER_ID).unwrap();
    let rows = retried.messages().to_vec();
    retried.commit();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0],
        String::from_utf8(reply.canonical_bytes())
            .unwrap()
            .parse::<JsonValue>()
            .unwrap()
    );
    let consumed = fixture
        .store
        .read_claim(&request.correlation_id)
        .unwrap()
        .unwrap();
    assert_eq!(consumed.state, ClaimState::ReplyConsumed);
    assert_eq!(consumed.reply_delivery, Some(DeliveryState::Consumed));
    assert!(
        lifecycle_drain(&fixture.home, REQUESTER_ID)
            .unwrap()
            .messages()
            .is_empty()
    );
}
