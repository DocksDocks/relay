#!/usr/bin/env node
// bus.mjs — zero-dependency MCP stdio server for the session-relay bus.
// Speaks newline-delimited JSON-RPC 2.0 on stdin/stdout (logs go to stderr).
// Implements the MCP lifecycle (initialize / notifications/initialized) and
// tools (tools/list, tools/call) over the shared store. Tools surface in
// Claude as mcp__plugin_session-relay_bus__<tool>.
//
// "Which session am I?" is resolved from the project dir (RELAY_PROJECT_DIR,
// set in the plugin manifest) via the cwd->id marker the SessionStart hook
// writes — the MCP protocol never hands a server the host's session id.
import * as store from '../lib/store.mjs';

const PROTOCOL = '2025-06-18';
// Resolve the project dir for self-id. Claude substitutes ${CLAUDE_PROJECT_DIR}
// in the manifest env; Codex config is static, so an unsubstituted "${...}" (or
// empty) is treated as absent and we fall back to the launch cwd — which Codex
// sets to the session's project dir, matching the dir its hook recorded.
const clean = (v) => (v && !v.includes('${') ? v : null);
const projectDir = clean(process.env.RELAY_PROJECT_DIR) || clean(process.env.CLAUDE_PROJECT_DIR) || process.cwd();
const log = (...a) => process.stderr.write(`[session-relay/bus] ${a.join(' ')}\n`);
const selfId = () => store.idForDir(projectDir);

const TOOLS = [
  {
    name: 'whoami',
    description: "Identify the session this bus is attached to (its registered session id, project dir, and friendly name).",
    inputSchema: { type: 'object', properties: {}, additionalProperties: false },
  },
  {
    name: 'register',
    description: 'Bind a friendly name to this session so others can address it by name instead of its raw session id.',
    inputSchema: {
      type: 'object',
      properties: {
        name: { type: 'string', description: 'Friendly name to claim, e.g. "frontend" or "agent-A".' },
        id: { type: 'string', description: 'Override session id (defaults to this session, resolved from the project dir).' },
        dir: { type: 'string', description: 'Override project dir (defaults to the launch dir).' },
      },
      required: ['name'],
      additionalProperties: false,
    },
  },
  {
    name: 'roster',
    description: 'List every registered session: name, session id, project dir, last-seen. Use to find a recipient.',
    inputSchema: { type: 'object', properties: {}, additionalProperties: false },
  },
  {
    name: 'send',
    description: "Queue a message to another session's inbox, addressed by friendly name or session id. The recipient reads it via inbox() or on its next session start; to deliver to an idle session now, wake it with relay.mjs.",
    inputSchema: {
      type: 'object',
      properties: {
        to: { type: 'string', description: 'Recipient friendly name or session id (see roster).' },
        body: { type: 'string', description: 'Message text.' },
      },
      required: ['to', 'body'],
      additionalProperties: false,
    },
  },
  {
    name: 'inbox',
    description: 'Read and clear this session\'s pending messages (each: from, body, ts).',
    inputSchema: { type: 'object', properties: {}, additionalProperties: false },
  },
];

const text = (obj, isError = false) => ({
  content: [{ type: 'text', text: typeof obj === 'string' ? obj : JSON.stringify(obj, null, 2) }],
  isError,
});

function callTool(name, args = {}) {
  switch (name) {
    case 'whoami': {
      const id = selfId();
      if (!id) return text({ registered: false, dir: projectDir, note: 'No session registered for this project dir yet — the SessionStart hook registers on session start/resume.' });
      return text({ registered: true, ...(store.resolve(id) || { id, dir: projectDir }) });
    }
    case 'register': {
      const id = args.id || selfId();
      if (!id) return text('Cannot register: no session id known for this project dir. Pass {id}, or ensure the SessionStart hook ran.', true);
      return text({ registered: true, ...store.register({ id, dir: args.dir || projectDir, name: args.name }) });
    }
    case 'roster':
      return text({ agents: store.roster() });
    case 'send': {
      if (!args.to || !args.body) return text('send requires {to, body}.', true);
      const target = store.resolve(String(args.to));
      if (!target) return text(`No session named or id "${args.to}" in the registry. Call roster to list recipients.`, true);
      const fromId = selfId();
      const from = fromId ? store.resolve(fromId) : null;
      store.enqueue(target.id, { from: fromId, fromName: from?.name || null, to: target.id, toName: target.name, body: String(args.body) });
      return text({
        ok: true,
        delivered_to: target.name || target.id,
        recipient_dir: target.dir,
        hint: `Recipient reads this via inbox() or on its next SessionStart. To wake an idle recipient now: node <plugin>/skills/productivity/session-relay/scripts/relay.mjs wake ${target.name || target.id}`,
      });
    }
    case 'inbox': {
      const id = selfId();
      if (!id) return text({ count: 0, messages: [], note: 'No session id for this project dir yet.' });
      const messages = store.drain(id);
      return text({ count: messages.length, messages });
    }
    default:
      throw { code: -32602, message: `Unknown tool: ${name}` };
  }
}

const send = (obj) => process.stdout.write(`${JSON.stringify(obj)}\n`);
const reply = (id, result) => send({ jsonrpc: '2.0', id, result });
const replyError = (id, code, message) => send({ jsonrpc: '2.0', id, error: { code, message } });

function handle(msg) {
  const { id, method, params } = msg;
  if (method === 'initialize') {
    return reply(id, {
      protocolVersion: params?.protocolVersion || PROTOCOL,
      capabilities: { tools: {} },
      serverInfo: { name: 'session-relay-bus', version: '0.1.0' },
      instructions: 'Cross-session message bus. Tools: whoami, register, roster, send, inbox.',
    });
  }
  if (method === 'notifications/initialized') return; // notification — no response
  if (method === 'ping') return reply(id, {});
  if (method === 'tools/list') return reply(id, { tools: TOOLS });
  if (method === 'tools/call') {
    try { return reply(id, callTool(params?.name, params?.arguments || {})); } catch (e) {
      if (e && typeof e.code === 'number') return replyError(id, e.code, e.message);
      return reply(id, text(`error: ${e?.message || e}`, true));
    }
  }
  if (id !== undefined) return replyError(id, -32601, `Method not found: ${method}`);
}

let buf = '';
process.stdin.setEncoding('utf8');
process.stdin.on('data', (chunk) => {
  buf += chunk;
  let nl;
  while ((nl = buf.indexOf('\n')) >= 0) {
    const line = buf.slice(0, nl).trim();
    buf = buf.slice(nl + 1);
    if (!line) continue;
    let msg;
    try { msg = JSON.parse(line); } catch { log('dropping non-JSON line'); continue; }
    try { handle(msg); } catch (e) { log('handler error:', e?.message || e); }
  }
});
process.stdin.on('end', () => process.exit(0));
log(`ready (project dir: ${projectDir})`);
