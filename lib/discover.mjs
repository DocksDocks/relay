// discover.mjs — find agent sessions that are running RIGHT NOW by scanning the
// raw on-disk session stores, so the bus can auto-resolve "my other session"
// with NO prior bus registration. The session-id↔cwd map a doorbell needs is
// already encoded on disk:
//   Claude: <root>/<encoded-cwd>/<session-id>.jsonl
//           — session id IS the filename; the dir name is a LOSSY encoding of cwd
//             (every non-alphanumeric → '-'), so the real cwd is read from the
//             file's content (the first line carrying a `cwd` field), never decoded
//             from the dir name.
//   Codex:  <root>/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl
//           — first line is a `session_meta` event whose payload has id + cwd.
// Liveness = file mtime recency. To keep cost proportional to LIVE sessions (not
// total history), files are stat-filtered by the liveness window BEFORE their
// content is read. Session ids must be UUID-shaped — both tools mint UUIDs, so a
// non-UUID id is a planted/garbage file and is dropped (it also keeps the id off
// the doorbell's argv as an injectable option). Roots honor each tool's own
// relocation env var — CLAUDE_CONFIG_DIR (-> <dir>/projects) and CODEX_HOME
// (-> <dir>/sessions) — falling back to ~/.claude/projects and ~/.codex/sessions;
// RELAY_CLAUDE_PROJECTS / RELAY_CODEX_SESSIONS override outright (tests).
// Zero deps; read-only (never mutates a store).
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import * as store from './store.mjs';

const claudeRoot = () => process.env.RELAY_CLAUDE_PROJECTS
  || path.join(process.env.CLAUDE_CONFIG_DIR || path.join(os.homedir(), '.claude'), 'projects');
const codexRoot = () => process.env.RELAY_CODEX_SESSIONS
  || path.join(process.env.CODEX_HOME || path.join(os.homedir(), '.codex'), 'sessions');

const READ_CAP = 65536; // bytes scanned per file to find cwd / parse the meta line
const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
const isUuid = (s) => typeof s === 'string' && UUID_RE.test(s);

function mtimeMs(file) {
  try { return fs.statSync(file).mtimeMs; } catch { return 0; }
}

// Read the first READ_CAP bytes of a file as whole lines (drops a trailing
// partial line, but never empties a single long line). Cheap bounded read —
// session transcripts can be megabytes.
function headLines(file) {
  let fd;
  try {
    fd = fs.openSync(file, 'r');
    const buf = Buffer.alloc(READ_CAP);
    const n = fs.readSync(fd, buf, 0, READ_CAP, 0);
    const lines = buf.subarray(0, n).toString('utf8').split('\n');
    if (n === READ_CAP && lines.length > 1) lines.pop(); // last line may be truncated
    return lines;
  } catch {
    return [];
  } finally {
    if (fd !== undefined) { try { fs.closeSync(fd); } catch { /* closed */ } }
  }
}

// Claude: the cwd lives in the file content, not the (lossy) dir name.
function claudeCwd(file) {
  for (const l of headLines(file)) {
    if (!l.trim() || !l.includes('"cwd"')) continue;
    try { const j = JSON.parse(l); if (j.cwd) return j.cwd; } catch { /* partial/other */ }
  }
  return null;
}

// Codex: the first line is the session_meta event (payload.id + payload.cwd).
function codexMeta(file) {
  for (const l of headLines(file)) {
    if (!l.trim()) continue;
    try {
      const j = JSON.parse(l);
      const p = j.payload || j;
      return { id: p.id || p.session_id || null, cwd: p.cwd || null };
    } catch { return null; }
  }
  return null;
}

// Cheap enumeration: list candidate session files with their mtime, WITHOUT
// reading content (content is read later, only for files inside the window).
function listClaudeFiles() {
  let projects;
  try { projects = fs.readdirSync(claudeRoot(), { withFileTypes: true }); } catch { return []; }
  const out = [];
  for (const proj of projects) {
    if (!proj.isDirectory()) continue;
    const pdir = path.join(claudeRoot(), proj.name);
    let ents;
    try { ents = fs.readdirSync(pdir, { withFileTypes: true }); } catch { continue; }
    for (const e of ents) {
      if (!e.isFile() || !e.name.endsWith('.jsonl')) continue;
      const file = path.join(pdir, e.name);
      out.push({ tool: 'claude', id: e.name.slice(0, -'.jsonl'.length), file, lastActivityMs: mtimeMs(file) });
    }
  }
  return out;
}
function listCodexFiles() {
  const out = [];
  (function walk(dir) {
    let ents;
    try { ents = fs.readdirSync(dir, { withFileTypes: true }); } catch { return; }
    for (const e of ents) {
      const full = path.join(dir, e.name);
      if (e.isDirectory()) walk(full);
      else if (e.isFile() && e.name.startsWith('rollout-') && e.name.endsWith('.jsonl')) {
        out.push({ tool: 'codex', id: null, file: full, lastActivityMs: mtimeMs(full) });
      }
    }
  }(codexRoot()));
  return out;
}

// Find live sessions, newest first. Options:
//   activeWithinMin  liveness window in minutes (default 60); older sessions dropped
//   tool             restrict to 'claude' | 'codex'
//   excludeId        drop this session id (the caller's own, so it never finds itself)
//   cwd              tie-breaker: a session whose cwd matches sorts first
//   limit            cap the result count (default 50)
export function discover({ activeWithinMin = 60, tool = null, excludeId = null, cwd = null, limit = 50 } = {}) {
  const now = Date.now();
  const cutoff = now - activeWithinMin * 60_000;
  // 1) cheap stat pass: enumerate + window-filter BEFORE reading any content.
  let files = [...listClaudeFiles(), ...listCodexFiles()];
  if (tool) files = files.filter((f) => f.tool === tool);
  files = files.filter((f) => f.lastActivityMs >= cutoff);
  files.sort((a, b) => b.lastActivityMs - a.lastActivityMs); // newest first → first id wins on dedupe
  // 2) content pass: only the windowed survivors get opened/parsed.
  const named = Object.fromEntries(store.roster().map((a) => [a.id, a]));
  const seen = new Set();
  const rows = [];
  for (const f of files) {
    let id = f.id;
    let fcwd = null;
    if (f.tool === 'claude') { fcwd = claudeCwd(f.file); } else {
      const m = codexMeta(f.file);
      if (m) { id = m.id; fcwd = m.cwd; }
    }
    if (!isUuid(id)) continue;          // planted/garbage id → skip (and keep it off the doorbell argv)
    if (excludeId && id === excludeId) continue;
    if (seen.has(id)) continue;         // files are newest-first, so first occurrence wins
    seen.add(id);
    const known = named[id];
    const ageSec = Math.max(0, Math.round((now - f.lastActivityMs) / 1000));
    rows.push({
      tool: f.tool,
      id,
      cwd: fcwd || known?.dir || null,
      name: known?.name || null,
      registered: !!known,
      lastActivity: new Date(f.lastActivityMs).toISOString(),
      ageSec,
      active: true, // window-filtered above
    });
  }
  if (cwd) rows.sort((a, b) => (a.cwd === cwd ? 0 : 1) - (b.cwd === cwd ? 0 : 1) || a.ageSec - b.ageSec);
  return rows.slice(0, limit);
}
