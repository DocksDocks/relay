// Black-box MCP lifecycle smoke test: spawn `relay bus` and speak real
// newline-delimited JSON-RPC over its stdio. Catches gross wire breakage long
// before the full Node selftest rewrite (rust-port plan step 6).

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use tinyjson::JsonValue;

fn obj(v: &JsonValue) -> &HashMap<String, JsonValue> {
    v.get::<HashMap<String, JsonValue>>().expect("object")
}

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
    let names: Vec<&str> = tools
        .iter()
        .map(|t| obj(t)["name"].get::<String>().unwrap().as_str())
        .collect();
    assert_eq!(
        names,
        ["whoami", "register", "roster", "send", "inbox", "discover"]
    );

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
