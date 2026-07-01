// store.mjs — shared on-disk state for the session-relay bus.
// Holds three things, all under one fixed home so every component agrees:
//   registry.json     id -> { id, dir, name, tool, lastSeen } + a name -> id index
//   mailbox/<id>.jsonl one append-only inbox per recipient session id
//   markers/<cwd>      the session id last registered for a project dir
// Consumed by the MCP server (mcp/bus.mjs), the per-tool SessionStart hooks, and
// relay.mjs. Shared across BOTH Claude Code and Codex sessions.
//
// Home is a FIXED, TOOL-NEUTRAL path (~/.agent-relay, not ${CLAUDE_PLUGIN_DATA})
// so relay.mjs — which runs via Bash with no plugin-variable substitution — and
// both tools' bus servers resolve the same store. Override with AGENT_RELAY_HOME;
// SESSION_RELAY_HOME is kept as a back-compat alias (v1 lived in ~/.claude).
//
// Cross-process safety: every mutation runs under an mkdir mutex. Registry and
// marker writes are atomic (tmp + rename); mailbox appends are serialized under
// that same mutex. Zero dependencies.
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import crypto from 'node:crypto';

export function homeDir() {
  return process.env.AGENT_RELAY_HOME
    || process.env.SESSION_RELAY_HOME
    || path.join(os.homedir(), '.agent-relay');
}
const P = (...p) => path.join(homeDir(), ...p);
const REGISTRY = () => P('registry.json');
const MAILBOX = (id) => P('mailbox', `${sanitize(id)}.jsonl`);
const MARKER = (dir) => P('markers', encodeDir(dir));
const LOCK = () => P('.lock');

// Filesystem-safe key for a project dir — mirrors Claude Code's own scheme
// (every non-alphanumeric char becomes '-').
export function encodeDir(dir) {
  return path.resolve(dir).replace(/[^a-zA-Z0-9]/g, '-');
}
const sanitize = (s) => String(s).replace(/[^a-zA-Z0-9._-]/g, '-');

function ensureDirs() {
  fs.mkdirSync(P('mailbox'), { recursive: true });
  fs.mkdirSync(P('markers'), { recursive: true });
}
function readJSON(file, fallback) {
  try { return JSON.parse(fs.readFileSync(file, 'utf8')); } catch { return fallback; }
}
function atomicWrite(file, text) {
  const tmp = `${file}.${process.pid}.${crypto.randomBytes(4).toString('hex')}.tmp`;
  fs.writeFileSync(tmp, text);
  fs.renameSync(tmp, file);
}
// Synchronous sleep with no deps — Atomics.wait is permitted on Node's main thread.
function sleepMs(ms) {
  Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms);
}

const STALE_MS = 10_000;
function withLock(fn) {
  ensureDirs();
  const lock = LOCK();
  const deadline = Date.now() + 3000;
  for (;;) {
    try { fs.mkdirSync(lock); break; } catch (e) {
      if (e.code !== 'EEXIST') throw e;
      // Bound EVERY path by the deadline — including stale reclaim — so a lock that
      // cannot be removed (e.g. rmdir keeps failing) fails fast instead of hanging.
      if (Date.now() > deadline) throw new Error('session-relay: lock busy (held > 3s)');
      let age = Infinity;
      try { age = Date.now() - fs.statSync(lock).mtimeMs; } catch { /* lock vanished — retry mkdir */ }
      if (age > STALE_MS) {
        // Reclaim atomically: rename the stale dir to a unique name first, so exactly
        // one racer wins (the rest get ENOENT) and only the winner removes it — two
        // writers can't both delete the lock and enter fn() concurrently.
        const abandoned = `${lock}.stale.${process.pid}.${crypto.randomBytes(4).toString('hex')}`;
        try { fs.renameSync(lock, abandoned); fs.rmdirSync(abandoned); }
        catch { sleepMs(25); } // lost the reclaim race or couldn't remove it — back off
        continue;
      }
      sleepMs(25);
    }
  }
  try { return fn(); } finally { try { fs.rmdirSync(lock); } catch { /* already gone */ } }
}

const emptyReg = () => ({ agents: {}, names: {} });

// Upsert a session. Missing fields are preserved from any prior entry, so the
// hook (id + dir, no name) and a later register(name) compose cleanly.
export function register({ id, dir, name, tool }) {
  if (!id) throw new Error('register requires an id');
  return withLock(() => {
    const reg = readJSON(REGISTRY(), emptyReg());
    const prev = reg.agents[id] || {};
    const entry = {
      id,
      dir: dir ? path.resolve(dir) : (prev.dir || null),
      name: name || prev.name || null,
      tool: tool || prev.tool || 'claude',
      lastSeen: new Date().toISOString(),
    };
    reg.agents[id] = entry;
    if (entry.name) {
      for (const [n, boundId] of Object.entries(reg.names)) {
        if (boundId === id && n !== entry.name) delete reg.names[n]; // drop a renamed alias
      }
      reg.names[entry.name] = id;
    }
    atomicWrite(REGISTRY(), JSON.stringify(reg, null, 2));
    return entry;
  });
}

export function roster() {
  const reg = readJSON(REGISTRY(), emptyReg());
  return Object.values(reg.agents)
    .sort((a, b) => (a.name || a.id).localeCompare(b.name || b.id));
}

// Resolve a target given either a friendly name or a raw session id.
export function resolve(nameOrId) {
  if (!nameOrId) return null;
  const reg = readJSON(REGISTRY(), emptyReg());
  if (reg.agents[nameOrId]) return reg.agents[nameOrId];
  const id = reg.names[nameOrId];
  return id ? (reg.agents[id] || null) : null;
}

export function setMarker(dir, id) {
  withLock(() => atomicWrite(MARKER(dir), `${id}\n`));
}
export function idForDir(dir) {
  try { return fs.readFileSync(MARKER(dir), 'utf8').trim() || null; } catch { return null; }
}

export function enqueue(recipientId, msg) {
  return withLock(() => {
    const line = JSON.stringify({ id: crypto.randomUUID(), ts: new Date().toISOString(), ...msg });
    fs.appendFileSync(MAILBOX(recipientId), `${line}\n`);
    return true;
  });
}

function parseLines(raw) {
  return raw.split('\n').filter(Boolean)
    .map((l) => { try { return JSON.parse(l); } catch { return null; } })
    .filter(Boolean);
}

// Read AND clear a recipient's inbox in one locked step.
export function drain(recipientId) {
  return withLock(() => {
    let raw = '';
    try { raw = fs.readFileSync(MAILBOX(recipientId), 'utf8'); } catch { return []; }
    const msgs = parseLines(raw);
    try { fs.rmSync(MAILBOX(recipientId)); } catch { /* already empty */ }
    return msgs;
  });
}

export function peek(recipientId) {
  try { return parseLines(fs.readFileSync(MAILBOX(recipientId), 'utf8')); } catch { return []; }
}
