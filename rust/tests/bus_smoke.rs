// Black-box MCP lifecycle smoke test: spawn `relay bus` and speak real
// newline-delimited JSON-RPC over its stdio. Catches gross wire breakage long
// before the full Node selftest rewrite (rust-port plan step 6).

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use tinyjson::JsonValue;

fn obj(v: &JsonValue) -> &HashMap<String, JsonValue> {
    v.get::<HashMap<String, JsonValue>>().expect("object")
}

fn tool_result(v: &JsonValue) -> &HashMap<String, JsonValue> {
    obj(&obj(v)["result"])
}

fn tool_text(v: &JsonValue) -> &str {
    let content = tool_result(v)["content"]
        .get::<Vec<JsonValue>>()
        .expect("tool result content");
    obj(&content[0])["text"]
        .get::<String>()
        .expect("text tool result")
}

fn assert_rpc_error(v: &JsonValue, code: f64) {
    assert_eq!(
        obj(&obj(v)["error"])["code"].get::<f64>().copied(),
        Some(code)
    );
}

fn assert_domain_error(v: &JsonValue, code: &str) {
    assert_eq!(tool_result(v)["isError"].get::<bool>().copied(), Some(true));
    assert_eq!(tool_text(v), format!(r#"{{"code":"{code}"}}"#));
}

fn assert_uuid_v4(value: &str) {
    assert!(relay::store::is_uuid(value), "not UUID-shaped: {value}");
    assert_eq!(value, value.to_ascii_lowercase(), "UUID is not lowercase");
    assert_eq!(&value[14..15], "4", "UUID is not version 4");
    assert!(
        matches!(&value[19..20], "8" | "9" | "a" | "b"),
        "UUID has an invalid variant"
    );
}

fn register_identity(home: &Path, name: &str, id: &str, dir: &Path) {
    let output = Command::new(env!("CARGO_BIN_EXE_relay"))
        .arg("register")
        .arg(name)
        .arg("--id")
        .arg(id)
        .arg("--dir")
        .arg(dir)
        .env("AGENT_RELAY_HOME", home)
        .env_remove("SESSION_RELAY_HOME")
        .output()
        .expect("run relay register");
    assert!(
        output.status.success(),
        "register {name} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

const LEGACY_INPUT_SCHEMAS: &str = r#"{
  "whoami":{"type":"object","properties":{},"additionalProperties":false},
  "register":{
    "type":"object",
    "properties":{
      "name":{"type":"string","description":"Friendly name to claim, e.g. \"frontend\" or \"agent-A\"."},
      "id":{"type":"string","description":"Override session id (defaults to this session, resolved from the project dir)."},
      "dir":{"type":"string","description":"Override project dir (defaults to the launch dir)."},
      "server":{"type":"string","description":"Codex app-server Unix socket for live delivery to this session."}
    },
    "required":["name"],
    "additionalProperties":false
  },
  "roster":{"type":"object","properties":{},"additionalProperties":false},
  "send":{
    "type":"object",
    "properties":{
      "to":{"type":"string","description":"Recipient friendly name or session id (see roster)."},
      "body":{"type":"string","description":"Message text."},
      "from":{"type":"string","description":"Your own registered session id or name (see the identity line injected at session start). Pass it whenever this project dir may host more than one session — the dir-marker fallback mis-attributes the sender in shared dirs."}
    },
    "required":["to","body"],
    "additionalProperties":false
  },
  "inbox":{
    "type":"object",
    "properties":{
      "id":{"type":"string","description":"Your own registered session id or name (see the identity line injected at session start). Pass it whenever this project dir may host more than one session — the dir-marker fallback can drain another session's mailbox."}
    },
    "additionalProperties":false
  },
  "discover":{
    "type":"object",
    "properties":{
      "activeWithinMin":{"type":"number","description":"Only sessions whose last activity is within this many minutes (default 60)."},
      "tool":{"type":"string","enum":["claude","codex"],"description":"Restrict to one tool."}
    },
    "additionalProperties":false
  }
}"#;

#[test]
fn bus_lifecycle_tools_and_whoami() {
    let home = std::env::temp_dir().join(format!(
        "relay-bus-smoke-{}-{}",
        std::process::id(),
        relay::store::uuid_v4()
    ));
    let pdir = home.join("project");
    fs::create_dir_all(&pdir).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_relay"))
        .arg("bus")
        .env("AGENT_RELAY_HOME", &home)
        .env("RELAY_PROJECT_DIR", &pdir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn relay bus");
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    fn rpc(
        stdin: &mut impl Write,
        lines: &mut impl Iterator<Item = std::io::Result<String>>,
        req: &str,
    ) -> JsonValue {
        writeln!(stdin, "{req}").unwrap();
        let line = lines.next().expect("a reply frame").expect("readable");
        line.parse().expect("reply is valid JSON")
    }

    // initialize echoes the client's protocolVersion
    let init = rpc(
        &mut stdin,
        &mut lines,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
    );
    let result = obj(&obj(&init)["result"]);
    assert_eq!(
        result["protocolVersion"].get::<String>().unwrap(),
        "2025-06-18"
    );
    assert_eq!(
        obj(&result["serverInfo"])["name"].get::<String>().unwrap(),
        "session-relay-bus"
    );

    // notifications/initialized gets NO reply — verify by pinging right after
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","method":"notifications/initialized"}}"#
    )
    .unwrap();
    let pong = rpc(
        &mut stdin,
        &mut lines,
        r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#,
    );
    assert_eq!(
        obj(&pong)["id"].get::<f64>().copied().unwrap(),
        2.0,
        "first frame after the notification must be the ping reply — the notification must not be answered"
    );

    // tools/list carries exactly the 6 tools
    let tl = rpc(
        &mut stdin,
        &mut lines,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/list"}"#,
    );
    let tools = obj(&obj(&tl)["result"])["tools"]
        .get::<Vec<JsonValue>>()
        .unwrap();
    let all_names: Vec<&str> = tools
        .iter()
        .map(|t| obj(t)["name"].get::<String>().unwrap().as_str())
        .collect();
    let names: Vec<&str> = all_names
        .iter()
        .copied()
        .filter(|name| !matches!(*name, "request" | "reply"))
        .collect();
    assert_eq!(
        names,
        ["whoami", "register", "roster", "send", "inbox", "discover"]
    );
    assert_eq!(
        all_names,
        [
            "whoami", "register", "roster", "send", "inbox", "discover", "request", "reply"
        ]
    );

    let expected_schemas: JsonValue = LEGACY_INPUT_SCHEMAS
        .parse()
        .expect("legacy MCP schemas are valid JSON");
    for tool in tools {
        let tool = obj(tool);
        let name = tool["name"].get::<String>().unwrap();
        if let Some(expected) = obj(&expected_schemas).get(name) {
            assert_eq!(
                &tool["inputSchema"], expected,
                "legacy {name} input schema changed"
            );
        }
    }

    for (name, expected_properties, expected_required) in [
        ("request", vec!["body", "from", "to"], vec!["to", "body"]),
        (
            "reply",
            vec!["body", "correlation_id", "from", "status"],
            vec!["correlation_id", "status", "body"],
        ),
    ] {
        let tool = tools
            .iter()
            .find(|tool| obj(tool)["name"].get::<String>().unwrap() == name)
            .expect("new MCP tool is listed");
        let schema = obj(&obj(tool)["inputSchema"]);
        let mut schema_keys = schema.keys().map(String::as_str).collect::<Vec<_>>();
        schema_keys.sort_unstable();
        assert_eq!(
            schema_keys,
            ["additionalProperties", "properties", "required", "type"]
        );
        assert_eq!(
            schema["type"].get::<String>().map(String::as_str),
            Some("object")
        );
        assert_eq!(
            schema["additionalProperties"].get::<bool>().copied(),
            Some(false)
        );
        let properties = obj(&schema["properties"]);
        let mut property_names = properties.keys().map(String::as_str).collect::<Vec<_>>();
        property_names.sort_unstable();
        assert_eq!(property_names, expected_properties);
        assert!(properties.values().all(|property| {
            obj(property)["type"].get::<String>().map(String::as_str) == Some("string")
        }));
        let required = schema["required"]
            .get::<Vec<JsonValue>>()
            .unwrap()
            .iter()
            .map(|value| value.get::<String>().unwrap().as_str())
            .collect::<Vec<_>>();
        assert_eq!(required, expected_required);
    }
    let reply_tool = tools
        .iter()
        .find(|tool| obj(tool)["name"].get::<String>().unwrap() == "reply")
        .unwrap();
    let reply_properties = obj(&obj(&obj(reply_tool)["inputSchema"])["properties"]);
    let statuses = obj(&reply_properties["status"])["enum"]
        .get::<Vec<JsonValue>>()
        .unwrap()
        .iter()
        .map(|value| value.get::<String>().unwrap().as_str())
        .collect::<Vec<_>>();
    assert_eq!(statuses, ["completed", "failed"]);

    // whoami with no marker: registered:false, non-error
    let who = rpc(
        &mut stdin,
        &mut lines,
        r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"whoami","arguments":{}}}"#,
    );
    let result = obj(&obj(&who)["result"]);
    assert!(!result["isError"].get::<bool>().copied().unwrap());
    let content = result["content"].get::<Vec<JsonValue>>().unwrap();
    let payload: JsonValue = obj(&content[0])["text"]
        .get::<String>()
        .unwrap()
        .parse()
        .expect("whoami text payload is JSON");
    assert!(!obj(&payload)["registered"].get::<bool>().copied().unwrap());

    // unknown tool → JSON-RPC error -32602
    let err = rpc(
        &mut stdin,
        &mut lines,
        r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"nope","arguments":{}}}"#,
    );
    assert_eq!(
        obj(&obj(&err)["error"])["code"]
            .get::<f64>()
            .copied()
            .unwrap(),
        -32602.0
    );

    drop(stdin); // EOF → clean exit
    let status = child.wait().expect("bus exits");
    assert!(status.success());
    fs::remove_dir_all(&home).ok();
}

fn tool_call_frame(id: u64, name: &str, arguments: &str) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"{name}","arguments":{arguments}}}}}"#
    )
}

fn assert_message_keys(message: &HashMap<String, JsonValue>) {
    let mut keys = message.keys().map(String::as_str).collect::<Vec<_>>();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "body",
            "correlation_id",
            "created_at",
            "from_session_id",
            "id",
            "kind",
            "reply_to",
            "result_sha256",
            "schema",
            "terminal_status",
            "to_session_id",
        ]
    );
}

#[test]
fn bus_request_reply_contract_and_domain_errors() {
    const REQUESTER_ID: &str = "a1111111-1111-4111-8111-111111111111";
    const RESPONDER_ID: &str = "b2222222-2222-4222-8222-222222222222";
    const OTHER_ID: &str = "c3333333-3333-4333-8333-333333333333";
    const UNKNOWN_CORRELATION: &str = "d4444444-4444-4444-8444-444444444444";

    let home = std::env::temp_dir().join(format!(
        "relay-bus-protocol-{}-{}",
        std::process::id(),
        relay::store::uuid_v4()
    ));
    let requester_dir = home.join("requester");
    let responder_dir = home.join("responder");
    let other_dir = home.join("other");
    for dir in [&requester_dir, &responder_dir, &other_dir] {
        fs::create_dir_all(dir).unwrap();
    }
    register_identity(&home, "mcp-a", REQUESTER_ID, &requester_dir);
    register_identity(&home, "mcp-b", RESPONDER_ID, &responder_dir);
    register_identity(&home, "mcp-c", OTHER_ID, &other_dir);

    let mut child = Command::new(env!("CARGO_BIN_EXE_relay"))
        .arg("bus")
        .env("AGENT_RELAY_HOME", &home)
        .env_remove("SESSION_RELAY_HOME")
        .env("RELAY_PROJECT_DIR", &requester_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn relay bus");
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    fn rpc(
        stdin: &mut impl Write,
        lines: &mut impl Iterator<Item = std::io::Result<String>>,
        req: &str,
    ) -> JsonValue {
        writeln!(stdin, "{req}").unwrap();
        lines
            .next()
            .expect("a reply frame")
            .expect("readable")
            .parse()
            .expect("reply is valid JSON")
    }

    let initialized = rpc(
        &mut stdin,
        &mut lines,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
    );
    assert_eq!(
        obj(&obj(&initialized)["result"])["protocolVersion"]
            .get::<String>()
            .map(String::as_str),
        Some("2025-06-18")
    );

    let missing_body = rpc(
        &mut stdin,
        &mut lines,
        &tool_call_frame(2, "request", r#"{"to":"mcp-b"}"#),
    );
    assert_rpc_error(&missing_body, -32602.0);
    let extra_argument = rpc(
        &mut stdin,
        &mut lines,
        &tool_call_frame(
            3,
            "request",
            r#"{"to":"mcp-b","body":"x","unexpected":true}"#,
        ),
    );
    assert_rpc_error(&extra_argument, -32602.0);
    let invalid_status = rpc(
        &mut stdin,
        &mut lines,
        &tool_call_frame(
            4,
            "reply",
            &format!(
                r#"{{"correlation_id":"{UNKNOWN_CORRELATION}","status":"done","body":"x","from":"mcp-b"}}"#
            ),
        ),
    );
    assert_rpc_error(&invalid_status, -32602.0);

    let request = rpc(
        &mut stdin,
        &mut lines,
        &tool_call_frame(
            5,
            "request",
            r#"{"to":"mcp-b","body":"need answer","from":"mcp-a"}"#,
        ),
    );
    assert_eq!(
        tool_result(&request)["isError"].get::<bool>().copied(),
        Some(false)
    );
    let request_text = tool_text(&request).to_string();
    let request_message: JsonValue = request_text.parse().expect("request payload is JSON");
    let request_message = obj(&request_message);
    assert_message_keys(request_message);
    assert_eq!(request_message["schema"].get::<f64>().copied(), Some(2.0));
    assert_eq!(
        request_message["kind"].get::<String>().map(String::as_str),
        Some("request")
    );
    assert_eq!(
        request_message["body"].get::<String>().map(String::as_str),
        Some("need answer")
    );
    assert_eq!(
        request_message["from_session_id"]
            .get::<String>()
            .map(String::as_str),
        Some(REQUESTER_ID)
    );
    assert_eq!(
        request_message["to_session_id"]
            .get::<String>()
            .map(String::as_str),
        Some(RESPONDER_ID)
    );
    assert_eq!(request_message["reply_to"], JsonValue::from(()));
    assert_eq!(request_message["terminal_status"], JsonValue::from(()));
    assert_eq!(request_message["result_sha256"], JsonValue::from(()));
    let correlation_id = request_message["correlation_id"]
        .get::<String>()
        .unwrap()
        .clone();
    let request_id = request_message["id"].get::<String>().unwrap().clone();
    let request_created_at = request_message["created_at"]
        .get::<String>()
        .unwrap()
        .clone();
    assert_uuid_v4(&correlation_id);
    assert_uuid_v4(&request_id);
    assert_eq!(
        request_text,
        format!(
            r#"{{"body":"need answer","correlation_id":"{correlation_id}","created_at":"{request_created_at}","from_session_id":"{REQUESTER_ID}","id":"{request_id}","kind":"request","reply_to":null,"result_sha256":null,"schema":2,"terminal_status":null,"to_session_id":"{RESPONDER_ID}"}}"#
        )
    );

    let unknown = rpc(
        &mut stdin,
        &mut lines,
        &tool_call_frame(
            6,
            "reply",
            &format!(
                r#"{{"correlation_id":"{UNKNOWN_CORRELATION}","status":"completed","body":"none","from":"mcp-b"}}"#
            ),
        ),
    );
    assert_domain_error(&unknown, "unknown_correlation");

    let unauthorized = rpc(
        &mut stdin,
        &mut lines,
        &tool_call_frame(
            7,
            "reply",
            &format!(
                r#"{{"correlation_id":"{correlation_id}","status":"completed","body":"forged","from":"mcp-c"}}"#
            ),
        ),
    );
    assert_domain_error(&unauthorized, "unauthorized_responder");

    let reply_arguments = format!(
        r#"{{"correlation_id":"{correlation_id}","status":"completed","body":"done","from":"mcp-b"}}"#
    );
    let reply = rpc(
        &mut stdin,
        &mut lines,
        &tool_call_frame(8, "reply", &reply_arguments),
    );
    assert_eq!(
        tool_result(&reply)["isError"].get::<bool>().copied(),
        Some(false)
    );
    let reply_text = tool_text(&reply).to_string();
    let reply_message: JsonValue = reply_text.parse().expect("reply payload is JSON");
    let reply_message = obj(&reply_message);
    assert_message_keys(reply_message);
    assert_eq!(
        reply_message["kind"].get::<String>().map(String::as_str),
        Some("terminal_reply")
    );
    assert_eq!(
        reply_message["from_session_id"]
            .get::<String>()
            .map(String::as_str),
        Some(RESPONDER_ID)
    );
    assert_eq!(
        reply_message["to_session_id"]
            .get::<String>()
            .map(String::as_str),
        Some(REQUESTER_ID)
    );
    assert_eq!(
        reply_message["reply_to"]
            .get::<String>()
            .map(String::as_str),
        Some(request_id.as_str())
    );
    assert_eq!(
        reply_message["terminal_status"]
            .get::<String>()
            .map(String::as_str),
        Some("completed")
    );
    assert_eq!(reply_message["result_sha256"], JsonValue::from(()));
    let reply_id = reply_message["id"].get::<String>().unwrap().clone();
    let reply_created_at = reply_message["created_at"].get::<String>().unwrap().clone();
    assert_uuid_v4(&reply_id);
    assert_eq!(
        reply_text,
        format!(
            r#"{{"body":"done","correlation_id":"{correlation_id}","created_at":"{reply_created_at}","from_session_id":"{RESPONDER_ID}","id":"{reply_id}","kind":"terminal_reply","reply_to":"{request_id}","result_sha256":null,"schema":2,"terminal_status":"completed","to_session_id":"{REQUESTER_ID}"}}"#
        )
    );

    let exact_retry = rpc(
        &mut stdin,
        &mut lines,
        &tool_call_frame(9, "reply", &reply_arguments),
    );
    assert_eq!(
        tool_result(&exact_retry)["isError"].get::<bool>().copied(),
        Some(false)
    );
    assert_eq!(tool_text(&exact_retry), reply_text);

    let competing = rpc(
        &mut stdin,
        &mut lines,
        &tool_call_frame(
            10,
            "reply",
            &format!(
                r#"{{"correlation_id":"{correlation_id}","status":"completed","body":"different","from":"mcp-b"}}"#
            ),
        ),
    );
    assert_domain_error(&competing, "correlation_conflict");

    drop(stdin);
    let status = child.wait().expect("bus exits");
    assert!(status.success());
    fs::remove_dir_all(&home).ok();
}

#[test]
fn bus_request_maps_protocol_store_failure_to_closed_domain_error() {
    const REQUESTER_ID: &str = "e5555555-5555-4555-8555-555555555555";
    const RESPONDER_ID: &str = "f6666666-6666-4666-8666-666666666666";
    let home = std::env::temp_dir().join(format!(
        "relay-bus-protocol-store-error-{}-{}",
        std::process::id(),
        relay::store::uuid_v4()
    ));
    let requester_dir = home.join("requester");
    let responder_dir = home.join("responder");
    fs::create_dir_all(&requester_dir).unwrap();
    fs::create_dir_all(&responder_dir).unwrap();
    register_identity(&home, "broken-a", REQUESTER_ID, &requester_dir);
    register_identity(&home, "broken-b", RESPONDER_ID, &responder_dir);
    fs::write(home.join("protocol-v1"), b"not a directory").unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_relay"))
        .arg("bus")
        .env("AGENT_RELAY_HOME", &home)
        .env_remove("SESSION_RELAY_HOME")
        .env("RELAY_PROJECT_DIR", &requester_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn relay bus");
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    fn rpc(
        stdin: &mut impl Write,
        lines: &mut impl Iterator<Item = std::io::Result<String>>,
        req: &str,
    ) -> JsonValue {
        writeln!(stdin, "{req}").unwrap();
        lines
            .next()
            .expect("a reply frame")
            .expect("readable")
            .parse()
            .expect("reply is valid JSON")
    }
    let _ = rpc(
        &mut stdin,
        &mut lines,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
    );
    let failure = rpc(
        &mut stdin,
        &mut lines,
        &tool_call_frame(
            2,
            "request",
            r#"{"to":"broken-b","body":"cannot persist","from":"broken-a"}"#,
        ),
    );
    assert_domain_error(&failure, "protocol_store_error");

    drop(stdin);
    let status = child.wait().expect("bus exits");
    assert!(status.success());
    fs::remove_dir_all(&home).ok();
}
