#!/usr/bin/env node
// selftest.mjs — exercises the session-relay machinery WITHOUT spawning a real
// `claude` session: it drives the actual MCP JSON-RPC handshake against bus.mjs,
// mutates the shared store, and feeds the SessionStart hook a real event.
// Runs against a throwaway SESSION_RELAY_HOME. Exit 0 = all assertions passed.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const PLUGIN = path.resolve(HERE, '..');
const BUS = path.join(PLUGIN, 'mcp/bus.mjs');
const HOOK = path.join(PLUGIN, 'hooks/session-start.mjs');
const RELAY = path.join(PLUGIN, 'skills/productivity/session-relay/scripts/relay.mjs');

const HOME = fs.mkdtempSync(path.join(os.tmpdir(), 'session-relay-test-'));
process.env.SESSION_RELAY_HOME = HOME;
const store = await import('../lib/store.mjs');

const dirA = path.join(HOME, 'proj-a');
const dirB = path.join(HOME, 'proj-b');
fs.mkdirSync(dirA, { recursive: true });
fs.mkdirSync(dirB, { recursive: true });
const idA = '11111111-1111-1111-1111-111111111111';
const idB = '22222222-2222-2222-2222-222222222222';

let passed = 0;
const check = (label, fn) => { fn(); passed += 1; console.log(`  ok: ${label}`); };

// Drive bus.mjs over stdio: write each request as one JSON line, collect the
// newline-delimited responses (notifications produce none).
function runBus(projectDir, requests) {
  const input = `${requests.map((r) => JSON.stringify(r)).join('\n')}\n`;
  const r = spawnSync('node', [BUS], {
    input, encoding: 'utf8',
    env: { ...process.env, SESSION_RELAY_HOME: HOME, RELAY_PROJECT_DIR: projectDir },
  });
  if (r.status !== 0 && r.status !== null) throw new Error(`bus exited ${r.status}: ${r.stderr}`);
  const byId = new Map();
  for (const line of (r.stdout || '').split('\n').filter(Boolean)) {
    const m = JSON.parse(line);
    if (m.id !== undefined) byId.set(m.id, m);
  }
  return byId;
}
const toolJSON = (resp) => JSON.parse(resp.result.content[0].text);

// --- store seed: register both sessions + markers (the hook does this live) ---
store.register({ id: idA, dir: dirA, name: 'agent-A' });
store.setMarker(dirA, idA);
store.register({ id: idB, dir: dirB, name: 'agent-B' });
store.setMarker(dirB, idB);

// --- MCP lifecycle + tools, as agent-A ---
const reqs = [
  { jsonrpc: '2.0', id: 1, method: 'initialize', params: { protocolVersion: '2025-06-18', capabilities: {}, clientInfo: { name: 'selftest', version: '1' } } },
  { jsonrpc: '2.0', method: 'notifications/initialized' },
  { jsonrpc: '2.0', id: 2, method: 'tools/list' },
  { jsonrpc: '2.0', id: 3, method: 'tools/call', params: { name: 'whoami', arguments: {} } },
  { jsonrpc: '2.0', id: 4, method: 'tools/call', params: { name: 'roster', arguments: {} } },
  { jsonrpc: '2.0', id: 5, method: 'tools/call', params: { name: 'send', arguments: { to: 'agent-B', body: 'hello from A' } } },
];
const res = runBus(dirA, reqs);

check('initialize negotiates protocol + serverInfo', () => {
  assert.equal(res.get(1).result.protocolVersion, '2025-06-18');
  assert.equal(res.get(1).result.serverInfo.name, 'session-relay-bus');
  assert.ok(res.get(1).result.capabilities.tools);
});
check('tools/list returns the 5 bus tools', () => {
  const names = res.get(2).result.tools.map((t) => t.name).sort();
  assert.deepEqual(names, ['inbox', 'register', 'roster', 'send', 'whoami']);
});
check('whoami resolves this session from the cwd marker', () => {
  const me = toolJSON(res.get(3));
  assert.equal(me.registered, true);
  assert.equal(me.id, idA);
  assert.equal(me.name, 'agent-A');
});
check('roster lists both registered sessions', () => {
  const { agents } = toolJSON(res.get(4));
  assert.deepEqual(agents.map((a) => a.name).sort(), ['agent-A', 'agent-B']);
});
check('send to agent-B reports ok + correct recipient dir', () => {
  const r = toolJSON(res.get(5));
  assert.equal(r.ok, true);
  assert.equal(r.delivered_to, 'agent-B');
  assert.equal(r.recipient_dir, dirB);
});
check("message landed in agent-B's mailbox tagged with the sender", () => {
  const mail = store.peek(idB);
  assert.equal(mail.length, 1);
  assert.equal(mail[0].body, 'hello from A');
  assert.equal(mail[0].fromName, 'agent-A');
});

// --- SessionStart hook for agent-B: registers + drains + injects context ---
const hookEv = JSON.stringify({ session_id: idB, cwd: dirB, hook_event_name: 'SessionStart', source: 'resume' });
const hookRun = spawnSync('node', [HOOK], { input: hookEv, encoding: 'utf8', env: { ...process.env, SESSION_RELAY_HOME: HOME } });
check('hook exits 0', () => assert.equal(hookRun.status, 0));
check('hook injects pending mail as SessionStart additionalContext', () => {
  const out = JSON.parse(hookRun.stdout);
  assert.equal(out.hookSpecificOutput.hookEventName, 'SessionStart');
  assert.ok(out.hookSpecificOutput.additionalContext.includes('hello from A'));
});
check('hook drained the inbox (no redelivery)', () => assert.equal(store.peek(idB).length, 0));

// --- inbox tool drains too: re-send, then read via the bus as agent-B ---
store.enqueue(idB, { from: idA, fromName: 'agent-A', to: idB, toName: 'agent-B', body: 'second message' });
const res2 = runBus(dirB, [
  { jsonrpc: '2.0', id: 1, method: 'initialize', params: { protocolVersion: '2025-06-18' } },
  { jsonrpc: '2.0', id: 2, method: 'tools/call', params: { name: 'inbox', arguments: {} } },
]);
check('inbox() returns then clears pending messages', () => {
  const box = toolJSON(res2.get(2));
  assert.equal(box.count, 1);
  assert.equal(box.messages[0].body, 'second message');
  assert.equal(store.peek(idB).length, 0);
});

// --- unknown recipient is a tool error, not a crash ---
const res3 = runBus(dirA, [
  { jsonrpc: '2.0', id: 1, method: 'initialize', params: {} },
  { jsonrpc: '2.0', id: 2, method: 'tools/call', params: { name: 'send', arguments: { to: 'ghost', body: 'x' } } },
]);
check('send to an unknown recipient returns isError', () => {
  assert.equal(res3.get(2).result.isError, true);
});

// --- v2: tool field + tool-aware doorbell dispatch + neutral home ---
const dirC = path.join(HOME, 'proj-c');
const idC = '33333333-3333-3333-3333-333333333333';
store.register({ id: idC, dir: dirC, name: 'codex-C', tool: 'codex' });
check('registry carries a tool field (codex tagged; default claude)', () => {
  assert.equal(store.resolve('codex-C').tool, 'codex');
  assert.equal(store.resolve('agent-A').tool, 'claude');
});
check('AGENT_RELAY_HOME takes precedence over SESSION_RELAY_HOME', () => {
  const saved = process.env.AGENT_RELAY_HOME;
  process.env.AGENT_RELAY_HOME = '/tmp/agent-relay-precedence';
  const h = store.homeDir();
  if (saved === undefined) delete process.env.AGENT_RELAY_HOME; else process.env.AGENT_RELAY_HOME = saved;
  assert.equal(h, '/tmp/agent-relay-precedence');
});
const relayDry = (who) => JSON.parse(spawnSync('node', [RELAY, 'wake', who, '--dry'],
  { encoding: 'utf8', env: { ...process.env, SESSION_RELAY_HOME: HOME } }).stdout);
check('wake dispatches the codex doorbell for a codex target', () => {
  const d = relayDry('codex-C');
  assert.equal(d.tool, 'codex');
  assert.equal(d.cmd, 'codex');
  assert.deepEqual(d.args.slice(0, 3), ['exec', 'resume', idC]);
  assert.equal(d.cwd, dirC);
});
check('wake dispatches the claude doorbell for a claude target', () => {
  const d = relayDry('agent-A');
  assert.equal(d.tool, 'claude');
  assert.equal(d.cmd, 'claude');
  assert.ok(d.args.includes('--resume') && d.args.includes(idA));
});

fs.rmSync(HOME, { recursive: true, force: true });
console.log(`\nPASS: session-relay self-test — ${passed} checks`);
