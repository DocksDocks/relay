#!/usr/bin/env node
// relay.mjs — session-relay CLI. The "doorbell" that wakes an idle session, plus
// manual registry/inbox ops over the shared store. Run by the session-relay
// skill (via Bash) or by a human. All commands are local; `wake` is the only one
// that spawns a process.
//
//   relay.mjs list
//   relay.mjs register <name> --id <uuid> [--dir <path>] [--tool claude|codex]
//   relay.mjs send <to> <message...>
//   relay.mjs inbox <nameOrId>
//   relay.mjs wake <nameOrId> [--dry] [message...]
//
// `wake` is TOOL-AWARE: it dispatches on the target's registered tool —
//   claude → `claude -p "<nudge>" --resume <id> --output-format json`
//   codex  → `codex exec resume <id> "<nudge>" --json`
// run from the target's registered project dir. That cwd matters: Claude scopes
// session-id lookup to the project dir (resuming elsewhere returns "No
// conversation found"); Codex is resumed from the dir its session was recorded
// in. `--dry` prints the command it would run instead of spawning (used by tests).
import { spawnSync } from 'node:child_process';
import * as store from '../../../../lib/store.mjs';

const argv = process.argv.slice(2);
const cmd = argv[0];
const die = (m) => { console.error(m); process.exit(1); };

function flag(name, fallback = null) {
  const i = argv.indexOf(`--${name}`);
  return i >= 0 && argv[i + 1] ? argv[i + 1] : fallback;
}
// positional args excluding flags + their values
function positionals(from) {
  const out = [];
  for (let i = from; i < argv.length; i += 1) {
    if (argv[i].startsWith('--')) { i += 1; continue; }
    out.push(argv[i]);
  }
  return out;
}

const DEFAULT_NUDGE = 'You have new session-relay mail. Use the session-relay skill: call inbox to read your pending messages and act on them.';

switch (cmd) {
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
    const [to, ...rest] = positionals(1);
    const body = rest.join(' ');
    if (!to || !body) die('usage: relay.mjs send <to> <message...>');
    const target = store.resolve(to);
    if (!target) die(`unknown recipient: ${to} (run: relay.mjs list)`);
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
    const [who, ...rest] = positionals(1);
    if (!who) die('usage: relay.mjs wake <nameOrId> [--dry] [message...]');
    const target = store.resolve(who);
    if (!target) die(`unknown session: ${who} (run: relay.mjs list)`);
    if (!target.id || !target.dir) die(`session ${who} is missing id/dir in the registry`);
    const message = rest.join(' ') || DEFAULT_NUDGE;
    const tool = target.tool || 'claude';
    // Per-tool headless-resume doorbell, run from the target's project dir.
    const doorbell = tool === 'codex'
      ? { cmd: 'codex', args: ['exec', 'resume', target.id, message, '--json'] }
      : { cmd: 'claude', args: ['-p', message, '--resume', target.id, '--output-format', 'json'] };
    if (argv.includes('--dry')) {
      console.log(JSON.stringify({ tool, cmd: doorbell.cmd, args: doorbell.args, cwd: target.dir }));
      break;
    }
    const r = spawnSync(doorbell.cmd, doorbell.args, { cwd: target.dir, encoding: 'utf8' });
    if (r.error) die(`failed to spawn ${doorbell.cmd}: ${r.error.message}`);
    if (r.stdout) process.stdout.write(r.stdout.endsWith('\n') ? r.stdout : `${r.stdout}\n`);
    if (r.stderr) process.stderr.write(r.stderr);
    process.exit(r.status ?? 0);
  }
  default:
    die('usage: relay.mjs list | register <name> --id <uuid> [--dir <path>] | send <to> <msg> | inbox <who> | wake <who> [msg]');
}
