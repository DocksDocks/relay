#!/usr/bin/env node
// relay.mjs — session-relay CLI. The "doorbell" that wakes an idle session, plus
// manual registry/inbox ops over the shared store. Run by the session-relay
// skill (via Bash) or by a human. All commands are local; `wake` is the only one
// that spawns a process.
//
//   relay.mjs discover [--within <min>] [--tool claude|codex] [--exclude <id>] [--cwd <path>] [--json]
//   relay.mjs list
//   relay.mjs register <name> --id <uuid> [--dir <path>] [--tool claude|codex]
//   relay.mjs send <to> <message...>            (or: send --id <id> <message...>)
//   relay.mjs inbox <nameOrId>
//   relay.mjs wake <nameOrId> [--dry] [message...]
//   relay.mjs wake --id <id> --dir <cwd> --tool <claude|codex> [message...]   (unregistered target)
//
// `discover` scans the live Claude + Codex session stores and lists sessions
// running now (newest first) — even ones that never joined the bus — so the
// agent can auto-resolve "my other session" without being handed an id.
//
// `wake` is TOOL-AWARE: it dispatches on the target's registered tool —
//   claude → `claude -p "<nudge>" --resume <id> --output-format json`
//   codex  → `codex exec resume <id> "<nudge>" --json`
// run from the target's registered project dir. That cwd matters: Claude scopes
// session-id lookup to the project dir (resuming elsewhere returns "No
// conversation found"); Codex is resumed from the dir its session was recorded
// in. `--dry` prints the command it would run instead of spawning (used by tests).
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import * as store from '../../../../lib/store.mjs';
import { discover } from '../../../../lib/discover.mjs';

const argv = process.argv.slice(2);
const cmd = argv[0];
const die = (m) => { console.error(m); process.exit(1); };

function flag(name, fallback = null) {
  const i = argv.indexOf(`--${name}`);
  return i >= 0 && argv[i + 1] ? argv[i + 1] : fallback;
}
// Valueless boolean flags — they do NOT consume the following token.
const BOOL_FLAGS = new Set(['dry', 'json']);
const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
// positional args excluding flags + their values; a bare `--` ends option parsing.
function positionals(from) {
  const out = [];
  for (let i = from; i < argv.length; i += 1) {
    const a = argv[i];
    if (a === '--') break; // end-of-options: everything after is the verbatim message
    if (a.startsWith('--')) {
      if (!BOOL_FLAGS.has(a.slice(2))) i += 1; // value flags also skip their value
      continue;
    }
    out.push(a);
  }
  return out;
}
// Message after an explicit `--` separator, verbatim (so a message may itself
// contain --flags without being mis-parsed); null when there is no separator.
function messageAfterSep() {
  const i = argv.indexOf('--');
  return i >= 0 ? argv.slice(i + 1).join(' ') : null;
}
// A target built straight from flags — addresses a discovered session that was
// never registered on the bus. Returns null when no --id is given. The id MUST be
// a session UUID: both tools mint UUIDs, and this keeps an attacker-planted,
// flag-shaped id (e.g. "--config=…") off the spawned doorbell's argv.
function explicitTarget() {
  const id = flag('id');
  if (!id) return null;
  if (!UUID_RE.test(id)) die(`--id must be a session UUID, got: ${id}`);
  return { id, dir: flag('dir') || process.cwd(), tool: flag('tool') || 'claude', name: null };
}

const DEFAULT_NUDGE = 'You have new session-relay mail. Use the session-relay skill: call inbox to read your pending messages and act on them.';

switch (cmd) {
  case 'discover': {
    const within = Number(flag('within', '60'));
    const rows = discover({
      activeWithinMin: Number.isFinite(within) ? within : 60,
      tool: flag('tool'),
      excludeId: flag('exclude'),
      cwd: flag('cwd'),
    });
    if (argv.includes('--json')) { console.log(JSON.stringify(rows, null, 2)); break; }
    if (!rows.length) { console.log(`(no active sessions in the last ${flag('within', '60')} min)`); break; }
    for (const r of rows) {
      console.log(`[${r.tool.padEnd(6)}] ${r.id}  ${r.cwd || '?'}  ${r.ageSec}s ago${r.name ? `  (${r.name})` : ''}${r.registered ? '' : '  [unregistered]'}`);
    }
    break;
  }
  case 'list': {
    const rows = store.roster();
    if (!rows.length) { console.log('(no sessions registered)'); break; }
    for (const r of rows) console.log(`${(r.name || '(unnamed)').padEnd(16)} [${(r.tool || 'claude').padEnd(6)}] ${r.id}  ${r.dir || '?'}  ${r.lastSeen || ''}`);
    break;
  }
  case 'register': {
    const name = positionals(1)[0];
    const id = flag('id');
    if (!name || !id) die('usage: relay.mjs register <name> --id <uuid> [--dir <path>] [--tool claude|codex]');
    const entry = store.register({ id, name, dir: flag('dir') || process.cwd(), tool: flag('tool') });
    console.log(`registered ${entry.name} [${entry.tool}] -> ${entry.id} @ ${entry.dir}`);
    break;
  }
  case 'send': {
    const explicit = explicitTarget();
    const rest = positionals(1);
    const to = explicit ? null : rest[0];
    const body = messageAfterSep() ?? (explicit ? rest : rest.slice(1)).join(' ');
    const target = explicit || (to ? store.resolve(to) : null);
    if (!target || !body) die('usage: relay.mjs send <to> [--] <message...>  (or: send --id <id> [--] <message...>)');
    store.enqueue(target.id, { from: null, fromName: 'cli', to: target.id, toName: target.name, body });
    console.log(`queued -> ${target.name || target.id}`);
    break;
  }
  case 'inbox': {
    const who = positionals(1)[0];
    if (!who) die('usage: relay.mjs inbox <nameOrId>');
    const target = store.resolve(who);
    if (!target) die(`unknown session: ${who}`);
    const msgs = store.drain(target.id);
    console.log(JSON.stringify({ count: msgs.length, messages: msgs }, null, 2));
    break;
  }
  case 'wake': {
    const explicit = explicitTarget();
    const rest = positionals(1);
    const who = explicit ? null : rest[0];
    const message = (messageAfterSep() ?? (explicit ? rest : rest.slice(1)).join(' ')) || DEFAULT_NUDGE;
    const target = explicit || (who ? store.resolve(who) : null);
    if (!target) die('usage: relay.mjs wake <nameOrId> [message...]  |  wake --id <id> --dir <cwd> --tool <claude|codex> [message...]');
    if (!target.id || !target.dir) die('target missing id/dir (for an unregistered session pass --dir)');
    const tool = target.tool || 'claude';
    // A registered target's id also lands on the spawned CLI's argv. explicitTarget()
    // already UUID-gates an --id; gate the resolved-name path too, so a planted,
    // flag-shaped id in the registry can't become an option.
    if (!UUID_RE.test(target.id)) die(`refusing to wake: target id is not a session UUID: ${target.id}`);
    // Per-tool headless-resume doorbell, run from the target's project dir. The
    // untrusted message goes AFTER a `--` end-of-options marker so a dash-leading
    // body can't be parsed as a flag on the child (both CLIs take the prompt as a
    // trailing positional; commander and clap both honor `--`).
    const doorbell = tool === 'codex'
      ? { cmd: 'codex', args: ['exec', 'resume', target.id, '--json', '--', message] }
      : { cmd: 'claude', args: ['-p', '--resume', target.id, '--output-format', 'json', '--', message] };
    if (argv.includes('--dry')) {
      console.log(JSON.stringify({ tool, cmd: doorbell.cmd, args: doorbell.args, cwd: target.dir }));
      break;
    }
    // Never resume into a cwd that no longer exists: a stale/moved registration
    // would otherwise resume from an unexpected dir (and Codex widens its sandbox
    // writable roots to the caller cwd). Refuse rather than spawn blindly.
    if (!fs.existsSync(target.dir)) die(`target dir does not exist: ${target.dir} — stale/moved session; re-register or pass the current --dir before waking.`);
    const r = spawnSync(doorbell.cmd, doorbell.args, { cwd: target.dir, encoding: 'utf8' });
    if (r.error) die(`failed to spawn ${doorbell.cmd}: ${r.error.message}`);
    if (r.stdout) process.stdout.write(r.stdout.endsWith('\n') ? r.stdout : `${r.stdout}\n`);
    if (r.stderr) process.stderr.write(r.stderr);
    process.exit(r.status ?? 0);
  }
  default:
    die('usage: relay.mjs discover [--within min] [--tool t] | list | register <name> --id <uuid> [--dir <path>] | send <to> <msg> | inbox <who> | wake <who> [msg]');
}
