#!/usr/bin/env node
// fake-app-server.mjs — minimal stand-in for `codex app-server --listen unix://…`
// used by selftest.mjs: a WebSocket server over a unix socket (the real thing
// speaks WS on every socket listener — spike-verified 2026-07-02 on codex-cli
// 0.142.5) that records every client JSON-RPC message to a JSONL file and
// answers just enough of the protocol for relay delivery and spawn:
//   initialize → result; thread/start → configured thread stub;
//   thread/resume → thread stub; thread/read → configured
//   status (idle by default); thread/inject_items → {};
//   turn/start → turn stub + turn/started, then an mcpServer/elicitation/request
//   for the bus server (MCP tool calls elicit approval from the connected
//   client regardless of approvalPolicy — live-verified); turn/completed is
//   emitted only after the client answers, mirroring the real wedge.
// Usage: node fake-app-server.mjs <socket-path> <frames-out.jsonl> [control.json]
// control.json: { "statuses": ["idle", "active", ...],
//   "threadId": "uuid", "threadStartError": "message",
//   "turnStartError": "message", "elicitationServer": "bus",
//   "reportedServiceTier": "default", "omitServiceTier": false,
//   "elicitationDelayMs": 0, "completionDelayMs": 0,
//   "neverComplete": false }
import crypto from 'node:crypto';
import fs from 'node:fs';
import net from 'node:net';

const [sock, framesFile, controlFile] = process.argv.slice(2);
if (!sock || !framesFile) {
  console.error('usage: fake-app-server.mjs <socket-path> <frames-out.jsonl> [control.json]');
  process.exit(1);
}
fs.rmSync(sock, { force: true });
const control = controlFile ? JSON.parse(fs.readFileSync(controlFile, 'utf8')) : {};
const statuses = Array.isArray(control.statuses) && control.statuses.length > 0
  ? control.statuses
  : ['idle'];
const elicitationServer = control.elicitationServer ?? 'bus';
const threadId = control.threadId ?? '77777777-7777-4777-8777-777777777777';
const turnId = control.turnId ?? 'turn-1';
const elicitationDelayMs = Number(control.elicitationDelayMs ?? 0);
const completionDelayMs = Number(control.completionDelayMs ?? 0);
const interruptCompletionDelayMs = Number(control.interruptCompletionDelayMs ?? 0);
let statusIndex = 0;
const effectiveServiceTier = (params) => Object.hasOwn(control, 'reportedServiceTier')
  ? control.reportedServiceTier
  : params?.serviceTier;

const encodeText = (s) => {
  const p = Buffer.from(s, 'utf8');
  let head;
  if (p.length < 126) head = Buffer.from([0x81, p.length]);
  else if (p.length <= 0xffff) { head = Buffer.alloc(4); head[0] = 0x81; head[1] = 126; head.writeUInt16BE(p.length, 2); }
  else { head = Buffer.alloc(10); head[0] = 0x81; head[1] = 127; head.writeBigUInt64BE(BigInt(p.length), 2); }
  return Buffer.concat([head, p]);
};

// { opcode, payload, used } | null when the buffer holds an incomplete frame
function parseFrame(buf) {
  if (buf.length < 2) return null;
  const opcode = buf[0] & 0x0f;
  const masked = (buf[1] & 0x80) !== 0;
  let len = buf[1] & 0x7f;
  let i = 2;
  if (len === 126) { if (buf.length < 4) return null; len = buf.readUInt16BE(2); i = 4; }
  else if (len === 127) { if (buf.length < 10) return null; len = Number(buf.readBigUInt64BE(2)); i = 10; }
  let mask = null;
  if (masked) { if (buf.length < i + 4) return null; mask = buf.subarray(i, i + 4); i += 4; }
  if (buf.length < i + len) return null;
  const payload = Buffer.from(buf.subarray(i, i + len));
  if (mask) for (let k = 0; k < payload.length; k += 1) payload[k] ^= mask[k % 4];
  return { opcode, payload, used: i + len };
}

const server = net.createServer((c) => {
  let buf = Buffer.alloc(0);
  let upgraded = false;
  let interrupted = false;
  let activeThreadId = threadId;
  const sendTurnCompleted = (completedThreadId = activeThreadId, completedTurnId = turnId) => {
    if (!c.destroyed) c.write(encodeText(JSON.stringify({
      method: 'turn/completed',
      params: {
        threadId: completedThreadId,
        turn: { id: completedTurnId, status: 'completed' },
      },
    })));
  };
  c.on('error', () => {});
  c.on('close', () => fs.appendFileSync(framesFile, `${JSON.stringify({ event: 'connection/closed' })}\n`));
  c.on('data', (d) => {
    buf = Buffer.concat([buf, d]);
    if (!upgraded) {
      const end = buf.indexOf('\r\n\r\n');
      if (end < 0) return;
      const head = buf.subarray(0, end).toString('utf8');
      const key = /^sec-websocket-key:\s*(.+)$/im.exec(head)?.[1].trim() ?? '';
      const accept = crypto.createHash('sha1').update(`${key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11`).digest('base64');
      c.write(`HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ${accept}\r\n\r\n`);
      buf = Buffer.from(buf.subarray(end + 4));
      upgraded = true;
    }
    let f;
    while ((f = parseFrame(buf))) {
      buf = Buffer.from(buf.subarray(f.used));
      if (f.opcode === 0x8) { c.end(); return; }
      if (f.opcode !== 0x1) continue; // ping/pong/binary: ignore
      let msg;
      try { msg = JSON.parse(f.payload.toString('utf8')); } catch { continue; }
      fs.appendFileSync(framesFile, `${JSON.stringify(msg)}\n`);
      if (msg.id === undefined) continue; // notification (e.g. initialized)
      if (msg.method === undefined) {
        // response to OUR elicitation request → the turn may finish
        if (msg.id === 990 && !control.neverComplete) {
          setTimeout(() => {
            sendTurnCompleted();
          }, completionDelayMs);
        }
        continue;
      }
      const reply = (result) => c.write(encodeText(JSON.stringify({ id: msg.id, result })));
      const fail = (message) => c.write(encodeText(JSON.stringify({ id: msg.id, error: { code: -32000, message } })));
      switch (msg.method) {
        case 'initialize': reply({ userAgent: 'fake-app-server/1.0' }); break;
        case 'thread/start': {
          if (control.threadStartError) fail(control.threadStartError);
          else {
            const result = {
              thread: { id: threadId, status: { type: 'idle' }, turns: [] },
              model: msg.params?.model ?? 'fake-model',
              modelProvider: 'fake-provider',
              cwd: msg.params?.cwd ?? process.cwd(),
              approvalPolicy: msg.params?.approvalPolicy ?? 'never',
              approvalsReviewer: 'user',
              sandbox: { type: 'workspaceWrite', writableRoots: [], networkAccess: false },
            };
            if (!control.omitServiceTier) result.serviceTier = effectiveServiceTier(msg.params);
            reply(result);
          }
          break;
        }
        case 'thread/resume': {
          const result = { thread: { id: msg.params?.threadId ?? null } };
          if (!control.omitServiceTier) result.serviceTier = effectiveServiceTier(msg.params);
          reply(result);
          break;
        }
        case 'thread/read': {
          const type = interrupted && control.idleAfterInterrupt
            ? 'idle'
            : statuses[Math.min(statusIndex, statuses.length - 1)];
          statusIndex += 1;
          const status = type === 'active' ? { type, activeFlags: [] } : { type };
          reply({ thread: { id: msg.params?.threadId ?? null, status, turns: [] } });
          break;
        }
        case 'thread/inject_items': reply({}); break;
        case 'turn/start':
          if (control.turnStartError) {
            fail(control.turnStartError);
            break;
          }
          activeThreadId = msg.params?.threadId ?? threadId;
          reply({ turn: { id: turnId, status: 'inProgress' } });
          c.write(encodeText(JSON.stringify({
            method: 'turn/started',
            params: {
              threadId: msg.params?.threadId ?? null,
              turn: { id: turnId, status: 'inProgress' },
            },
          })));
          if (control.disconnectAfterTurnStart) {
            setTimeout(() => c.destroy(), 10);
            break;
          }
          setTimeout(() => {
            if (!c.destroyed) c.write(encodeText(JSON.stringify({ id: 990, method: 'mcpServer/elicitation/request', params: { threadId: msg.params?.threadId ?? null, turnId, serverName: elicitationServer, mode: 'form', _meta: { codex_approval_kind: 'mcp_tool_call' } } })));
          }, elicitationDelayMs);
          break;
        case 'turn/interrupt':
          if (msg.params?.threadId !== activeThreadId || msg.params?.turnId !== turnId) {
            fail('turn interrupt identity mismatch');
            break;
          }
          interrupted = true;
          reply({});
          if (control.interruptMismatchedCompletion) {
            sendTurnCompleted(activeThreadId, `${turnId}-mismatch`);
          }
          if (!control.interruptNeverComplete) {
            setTimeout(() => sendTurnCompleted(), interruptCompletionDelayMs);
          }
          break;
        default: reply({});
      }
    }
  });
});
server.listen(sock, () => console.log('READY'));
