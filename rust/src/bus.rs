// bus.rs — MCP stdio server for the session-relay bus (port of mcp/bus.mjs).
// Speaks newline-delimited JSON-RPC 2.0 on stdin/stdout. STDOUT PURITY IS A
// SPEC MUST: nothing but JSON-RPC frames goes to stdout (MCP stdio transport,
// 2025-06-18) — every diagnostic goes through log() to stderr. Implements the
// MCP lifecycle (initialize / notifications/initialized / ping) and tools
// (tools/list, tools/call) over the shared store.
//
// "Which session am I?" is resolved from the project dir (RELAY_PROJECT_DIR,
// set in the plugin manifest) via the cwd->id marker the SessionStart hook
// writes — the MCP protocol never hands a server the host's session id.

use crate::discover;
use crate::gc;
use crate::lifecycle::{self, OperationKind};
use crate::store;
use std::collections::HashMap;
use std::io::{BufRead, Write};
use tinyjson::JsonValue;

const PROTOCOL: &str = "2025-06-18";

// The 6 tool schemas, verbatim from the Node bus (wire-identical surface).
const TOOLS_JSON: &str = r#"[
  {
    "name": "whoami",
    "description": "Identify the session this bus is attached to (its registered session id, project dir, and friendly name).",
    "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
  },
  {
    "name": "register",
    "description": "Bind a friendly name to this session so others can address it by name instead of its raw session id.",
    "inputSchema": {
      "type": "object",
      "properties": {
        "name": { "type": "string", "description": "Friendly name to claim, e.g. \"frontend\" or \"agent-A\"." },
        "id": { "type": "string", "description": "Override session id (defaults to this session, resolved from the project dir)." },
        "dir": { "type": "string", "description": "Override project dir (defaults to the launch dir)." },
        "server": { "type": "string", "description": "Codex app-server Unix socket for live delivery to this session." }
      },
      "required": ["name"],
      "additionalProperties": false
    }
  },
  {
    "name": "roster",
    "description": "List every registered session: name, session id, project dir, last-seen. Use to find a recipient.",
    "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
  },
  {
    "name": "send",
    "description": "Queue a message to another session's inbox, addressed by friendly name or session id. The recipient reads it via inbox() or on its next session start; to deliver to an idle session now, wake it with the relay CLI.",
    "inputSchema": {
      "type": "object",
      "properties": {
        "to": { "type": "string", "description": "Recipient friendly name or session id (see roster)." },
        "body": { "type": "string", "description": "Message text." },
        "from": { "type": "string", "description": "Your own registered session id or name (see the identity line injected at session start). Pass it whenever this project dir may host more than one session — the dir-marker fallback mis-attributes the sender in shared dirs." }
      },
      "required": ["to", "body"],
      "additionalProperties": false
    }
  },
  {
    "name": "inbox",
    "description": "Read and clear this session's pending messages (each: from, body, ts).",
    "inputSchema": {
      "type": "object",
      "properties": {
        "id": { "type": "string", "description": "Your own registered session id or name (see the identity line injected at session start). Pass it whenever this project dir may host more than one session — the dir-marker fallback can drain another session's mailbox." }
      },
      "additionalProperties": false
    }
  },
  {
    "name": "discover",
    "description": "Find other agent sessions running RIGHT NOW (Claude or Codex) by scanning the on-disk session stores — works even for sessions that never registered on the bus. Returns candidates ranked by recency (sessions in this same project dir first), each with {tool, id, cwd, name, registered, ageSec, active}. Use this to auto-locate \"my other session\" without being handed an id; then send()+wake it, or wake an unregistered one directly with its id/dir/tool.",
    "inputSchema": {
      "type": "object",
      "properties": {
        "activeWithinMin": { "type": "number", "description": "Only sessions whose last activity is within this many minutes (default 60)." },
        "tool": { "type": "string", "enum": ["claude", "codex"], "description": "Restrict to one tool." }
      },
      "additionalProperties": false
    }
  }
]"#;

fn log(msg: &str) {
    eprintln!("[session-relay/bus] {msg}");
}

// Resolve the project dir for self-id. Claude substitutes ${CLAUDE_PROJECT_DIR}
// in the manifest env; Codex config is static, so an unsubstituted "${...}" (or
// empty) is treated as absent and we fall back to the launch cwd.
fn project_dir() -> String {
    let clean = |var: &str| {
        std::env::var(var)
            .ok()
            .filter(|v| !v.is_empty() && !v.contains("${"))
    };
    clean("RELAY_PROJECT_DIR")
        .or_else(|| clean("CLAUDE_PROJECT_DIR"))
        .unwrap_or_else(|| {
            std::env::current_dir()
                .map(|d| d.to_string_lossy().into_owned())
                .unwrap_or_else(|_| ".".to_string())
        })
}

fn obj(entries: Vec<(&str, JsonValue)>) -> JsonValue {
    let mut m: HashMap<String, JsonValue> = HashMap::new();
    for (k, v) in entries {
        m.insert(k.to_string(), v);
    }
    JsonValue::from(m)
}
fn js(s: impl Into<String>) -> JsonValue {
    JsonValue::from(s.into())
}
fn jnull() -> JsonValue {
    JsonValue::from(())
}

// MCP tool result: {content: [{type: "text", text}], isError}.
fn text(payload: JsonValue, is_error: bool) -> JsonValue {
    let s = match payload.get::<String>() {
        Some(raw) => raw.clone(),
        None => payload.format().unwrap_or_else(|_| "{}".to_string()),
    };
    obj(vec![
        (
            "content",
            JsonValue::from(vec![obj(vec![("type", js("text")), ("text", js(s))])]),
        ),
        ("isError", JsonValue::from(is_error)),
    ])
}

enum ToolErr {
    Rpc(f64, String),
    Soft(String),
}

fn drain_inbox(id: &str) -> Result<Vec<JsonValue>, ToolErr> {
    let mut guard = lifecycle::admit_operation(id, OperationKind::McpInboxDrain)
        .and_then(lifecycle::Admission::into_guard)
        .map_err(ToolErr::Soft)?;
    lifecycle::drain_with_guard(&mut guard)
        .map(store::DrainReceipt::into_messages)
        .map_err(ToolErr::Soft)
}

fn arg_str(args: &HashMap<String, JsonValue>, key: &str) -> Option<String> {
    args.get(key)?
        .get::<String>()
        .filter(|s| !s.is_empty())
        .cloned()
}

// Flatten an Entry under {registered: true, ...entry} like the JS spread.
fn registered_entry(e: &store::Entry) -> JsonValue {
    let mut m = e
        .to_json()
        .get::<HashMap<String, JsonValue>>()
        .cloned()
        .unwrap_or_default();
    m.insert("registered".into(), JsonValue::from(true));
    JsonValue::from(m)
}

fn call_tool(
    name: &str,
    args: &HashMap<String, JsonValue>,
    pdir: &str,
) -> Result<JsonValue, ToolErr> {
    let self_id = || store::id_for_dir(pdir);
    match name {
        "whoami" => {
            let Some(id) = self_id() else {
                return Ok(text(
                    obj(vec![
                        ("registered", JsonValue::from(false)),
                        ("dir", js(pdir)),
                        (
                            "note",
                            js(
                                "No session registered for this project dir yet — the SessionStart hook registers on session start/resume.",
                            ),
                        ),
                    ]),
                    false,
                ));
            };
            match store::resolve(&id) {
                Some(e) => Ok(text(registered_entry(&e), false)),
                None => Ok(text(
                    obj(vec![
                        ("registered", JsonValue::from(true)),
                        ("id", js(id)),
                        ("dir", js(pdir)),
                    ]),
                    false,
                )),
            }
        }
        "register" => {
            let id = arg_str(args, "id").or_else(self_id);
            let Some(id) = id else {
                return Ok(text(
                    js(
                        "Cannot register: no session id known for this project dir. Pass {id}, or ensure the SessionStart hook ran.",
                    ),
                    true,
                ));
            };
            let dir = arg_str(args, "dir").unwrap_or_else(|| pdir.to_string());
            let name = arg_str(args, "name");
            let server = arg_str(args, "server");
            let entry = store::register(&id, Some(&dir), name.as_deref(), None, server.as_deref())
                .map_err(ToolErr::Soft)?;
            Ok(text(registered_entry(&entry), false))
        }
        "roster" => {
            let agents: Vec<JsonValue> =
                store::roster().iter().map(store::Entry::to_json).collect();
            Ok(text(obj(vec![("agents", JsonValue::from(agents))]), false))
        }
        "send" => {
            let (Some(to), Some(body)) = (arg_str(args, "to"), arg_str(args, "body")) else {
                return Ok(text(js("send requires {to, body}."), true));
            };
            let Some(target) = store::resolve(&to) else {
                return Ok(text(
                    js(format!(
                        "No session named or id \"{to}\" in the registry. Call roster to list recipients."
                    )),
                    true,
                ));
            };
            // Explicit sender identity (the (b) handshake): validated against
            // the registry so a typo can't forge or silently mis-attribute.
            // Absent -> the dir-marker fallback (correct for single-session dirs).
            let (from_id, from_name) = match arg_str(args, "from") {
                Some(f) => {
                    let Some(e) = store::resolve(&f) else {
                        return Ok(text(
                            js(format!(
                                "Unknown \"from\" identity \"{f}\" — pass your own registered session id or name (see the identity line injected at session start, or roster)."
                            )),
                            true,
                        ));
                    };
                    (Some(e.id), e.name)
                }
                None => {
                    let id = self_id();
                    let name = id.as_deref().and_then(store::resolve).and_then(|e| e.name);
                    (id, name)
                }
            };
            let mut msg: HashMap<String, JsonValue> = HashMap::new();
            msg.insert("from".into(), from_id.map(js).unwrap_or_else(jnull));
            msg.insert("fromName".into(), from_name.map(js).unwrap_or_else(jnull));
            msg.insert("to".into(), js(target.id.clone()));
            msg.insert(
                "toName".into(),
                target.name.clone().map(js).unwrap_or_else(jnull),
            );
            msg.insert("body".into(), js(body));
            store::enqueue(&target.id, &msg).map_err(ToolErr::Soft)?;
            let addressee = target.name.clone().unwrap_or_else(|| target.id.clone());
            Ok(text(
                obj(vec![
                    ("ok", JsonValue::from(true)),
                    ("delivered_to", js(addressee.clone())),
                    (
                        "recipient_dir",
                        target.dir.clone().map(js).unwrap_or_else(jnull),
                    ),
                    (
                        "recipient_watch",
                        js(store::watcher_status(&target.id).as_str()),
                    ),
                    (
                        "hint",
                        js(format!(
                            "Recipient reads this via inbox() or on its next SessionStart. To wake an idle recipient now: <plugin>/bin/relay wake {addressee}"
                        )),
                    ),
                ]),
                false,
            ))
        }
        "inbox" => {
            // Same handshake as send's `from`: an explicit id keeps a shared
            // dir's sessions from draining each other's mail via the marker.
            if let Some(who) = arg_str(args, "id") {
                let Some(e) = store::resolve(&who) else {
                    return Ok(text(
                        js(format!(
                            "Unknown inbox identity \"{who}\" — pass your own registered session id or name (see the identity line injected at session start, or roster)."
                        )),
                        true,
                    ));
                };
                let messages = drain_inbox(&e.id)?;
                return Ok(text(
                    obj(vec![
                        ("count", JsonValue::from(messages.len() as f64)),
                        ("messages", JsonValue::from(messages)),
                    ]),
                    false,
                ));
            }
            let Some(id) = self_id() else {
                return Ok(text(
                    obj(vec![
                        ("count", JsonValue::from(0.0)),
                        ("messages", JsonValue::from(Vec::<JsonValue>::new())),
                        ("note", js("No session id for this project dir yet.")),
                    ]),
                    false,
                ));
            };
            let messages = drain_inbox(&id)?;
            Ok(text(
                obj(vec![
                    ("count", JsonValue::from(messages.len() as f64)),
                    ("messages", JsonValue::from(messages)),
                ]),
                false,
            ))
        }
        "discover" => {
            let within = args
                .get("activeWithinMin")
                .and_then(|v| v.get::<f64>().copied())
                .unwrap_or(60.0);
            let exclude = self_id();
            let tool_arg = arg_str(args, "tool"); // raw — the filter is equality, like the Node bus
            let sessions = discover::discover(&discover::Options {
                active_within_min: within,
                tool: tool_arg.as_deref(),
                exclude_id: exclude.as_deref(),
                cwd: Some(pdir),
                ..Default::default()
            });
            Ok(text(
                obj(vec![
                    ("count", JsonValue::from(sessions.len() as f64)),
                    ("sessions", JsonValue::from(sessions)),
                    (
                        "note",
                        js(
                            "Ranked by recency (this project dir first). To reach one: send() then wake it via the relay CLI; for an unregistered session pass its id/dir/tool to `<plugin>/bin/relay wake`.",
                        ),
                    ),
                ]),
                false,
            ))
        }
        other => Err(ToolErr::Rpc(-32602.0, format!("Unknown tool: {other}"))),
    }
}

fn send_frame(v: JsonValue) {
    if let Ok(s) = v.stringify() {
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{s}");
        let _ = out.flush();
    }
}
fn reply(id: JsonValue, result: JsonValue) {
    send_frame(obj(vec![
        ("jsonrpc", js("2.0")),
        ("id", id),
        ("result", result),
    ]));
}
fn reply_error(id: JsonValue, code: f64, message: String) {
    send_frame(obj(vec![
        ("jsonrpc", js("2.0")),
        ("id", id),
        (
            "error",
            obj(vec![
                ("code", JsonValue::from(code)),
                ("message", js(message)),
            ]),
        ),
    ]));
}

fn handle(msg: &JsonValue, pdir: &str) {
    let Some(m) = msg.get::<HashMap<String, JsonValue>>() else {
        return;
    };
    let id = m.get("id").cloned();
    let method = m
        .get("method")
        .and_then(|v| v.get::<String>().cloned())
        .unwrap_or_default();
    let params = m
        .get("params")
        .and_then(|v| v.get::<HashMap<String, JsonValue>>());
    match method.as_str() {
        "initialize" => {
            let client_proto = params
                .and_then(|p| p.get("protocolVersion"))
                .and_then(|v| v.get::<String>().cloned())
                .unwrap_or_else(|| PROTOCOL.to_string());
            reply(
                id.unwrap_or_else(jnull),
                obj(vec![
                    ("protocolVersion", js(client_proto)),
                    ("capabilities", obj(vec![("tools", obj(vec![]))])),
                    (
                        "serverInfo",
                        obj(vec![
                            ("name", js("session-relay-bus")),
                            ("version", js("0.1.0")),
                        ]),
                    ),
                    (
                        "instructions",
                        js(
                            "Cross-session message bus. Tools: whoami, register, roster, send, inbox, discover.",
                        ),
                    ),
                ]),
            );
        }
        "notifications/initialized" => {} // notification — no response
        "ping" => reply(id.unwrap_or_else(jnull), obj(vec![])),
        "tools/list" => {
            let tools: JsonValue = TOOLS_JSON.parse().expect("TOOLS_JSON is valid");
            reply(id.unwrap_or_else(jnull), obj(vec![("tools", tools)]));
        }
        "tools/call" => {
            let name = params
                .and_then(|p| p.get("name"))
                .and_then(|v| v.get::<String>().cloned())
                .unwrap_or_default();
            let empty = HashMap::new();
            let args = params
                .and_then(|p| p.get("arguments"))
                .and_then(|v| v.get::<HashMap<String, JsonValue>>())
                .unwrap_or(&empty);
            match call_tool(&name, args, pdir) {
                Ok(result) => reply(id.unwrap_or_else(jnull), result),
                Err(ToolErr::Rpc(code, message)) => {
                    reply_error(id.unwrap_or_else(jnull), code, message)
                }
                Err(ToolErr::Soft(e)) => reply(
                    id.unwrap_or_else(jnull),
                    text(js(format!("error: {e}")), true),
                ),
            }
        }
        other => {
            if let Some(id) = id {
                reply_error(id, -32601.0, format!("Method not found: {other}"));
            }
        }
    }
}

pub fn run() -> ! {
    let pdir = project_dir();
    let self_id = store::id_for_dir(&pdir);
    if let Err(e) = gc::run(std::time::SystemTime::now(), self_id.as_deref()) {
        log(&format!("GC skipped: {e}"));
    }
    log(&format!("ready (project dir: {pdir})"));
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match line.parse::<JsonValue>() {
            Ok(msg) => handle(&msg, &pdir),
            Err(_) => log("dropping non-JSON line"),
        }
    }
    std::process::exit(0);
}
